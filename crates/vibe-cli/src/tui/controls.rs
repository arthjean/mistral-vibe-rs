use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallbackKind {
    Approval,
    UserInput,
    PlanReview,
}

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
    pub kind: CallbackKind,
    pub prompt: String,
    pub options: Vec<CallbackOption>,
    pub allows_free_text: bool,
    pub multi_select: bool,
    pub questions: Vec<CallbackQuestion>,
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
    pending: BTreeMap<String, PendingCallback>,
    answered: BTreeMap<String, AnsweredCallback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnsweredCallback {
    choice: CallbackChoice,
    params: Value,
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
            pending: BTreeMap::new(),
            answered: BTreeMap::new(),
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
        if callback.session_id != self.session_id {
            return Err(ControlError::ForeignSession(callback.session_id));
        }
        if self.active_turn_id.as_deref() != Some(&callback.turn_id) {
            return Err(ControlError::StaleTurn(callback.turn_id));
        }
        if self.pending.contains_key(&callback.callback_id)
            || self.answered.contains_key(&callback.callback_id)
        {
            return Err(ControlError::DuplicateCallback(callback.callback_id));
        }
        self.focus = match callback.kind {
            CallbackKind::PlanReview => ControlFocus::Plan,
            CallbackKind::Approval | CallbackKind::UserInput => ControlFocus::Callback,
        };
        self.waiting = true;
        self.pending.insert(callback.callback_id.clone(), callback);
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
            .get(callback_id)
            .ok_or_else(|| ControlError::StaleCallback(callback_id.to_owned()))?;
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
        self.pending.remove(callback_id);
        self.answered.insert(
            callback_id.to_owned(),
            AnsweredCallback {
                choice,
                params: prepared.params,
            },
        );
        self.focus = ControlFocus::Prompt;
        self.waiting = true;
        Ok(())
    }

    #[must_use]
    pub fn pending_callback(&self) -> Option<&PendingCallback> {
        self.pending.values().next()
    }

    #[must_use]
    pub fn contains_callback(&self, callback_id: &str) -> bool {
        self.pending.contains_key(callback_id) || self.answered.contains_key(callback_id)
    }

    pub fn reconcile_active_callbacks(&mut self, active_callback_ids: &[&str]) {
        self.pending
            .retain(|callback_id, _| active_callback_ids.contains(&callback_id.as_str()));
        if self.pending.is_empty()
            && matches!(self.focus, ControlFocus::Callback | ControlFocus::Plan)
        {
            self.focus = ControlFocus::Prompt;
        }
    }

    pub fn interrupt(&mut self) -> Result<ControlDispatch, ControlError> {
        let turn_id = self
            .active_turn_id
            .as_ref()
            .ok_or(ControlError::NoActiveTurn)?
            .clone();
        self.pending.clear();
        self.focus = ControlFocus::Prompt;
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
        self.pending.clear();
        self.focus = ControlFocus::Prompt;
        self.waiting = false;
        self.notifications.push(message.into());
    }

    #[must_use]
    pub fn session_command(&self, command: SessionCommand) -> ControlDispatch {
        command.dispatch(&self.session_id)
    }
}

fn validate_choice(pending: &PendingCallback, choice: &CallbackChoice) -> Result<(), ControlError> {
    match choice {
        CallbackChoice::Approve { .. } | CallbackChoice::Deny { .. }
            if pending.kind != CallbackKind::Approval =>
        {
            Err(ControlError::InvalidChoice)
        }
        CallbackChoice::Option { id } if !pending.options.iter().any(|option| option.id == *id) => {
            Err(ControlError::InvalidChoice)
        }
        CallbackChoice::Options { ids }
            if !pending.multi_select
                || ids.is_empty()
                || ids
                    .iter()
                    .enumerate()
                    .any(|(index, id)| ids[..index].contains(id))
                || ids
                    .iter()
                    .any(|id| !pending.options.iter().any(|option| option.id == *id)) =>
        {
            Err(ControlError::InvalidChoice)
        }
        CallbackChoice::FreeText { value }
            if !pending.allows_free_text || value.trim().is_empty() =>
        {
            Err(ControlError::InvalidChoice)
        }
        CallbackChoice::UserInput { answers }
            if pending.kind != CallbackKind::UserInput
                || answers.len() != pending.questions.len()
                || answers
                    .iter()
                    .zip(&pending.questions)
                    .any(|(answer, question)| !valid_question_answer(question, answer)) =>
        {
            Err(ControlError::InvalidChoice)
        }
        CallbackChoice::Cancel => Ok(()),
        _ => Ok(()),
    }
}

fn valid_question_answer(question: &CallbackQuestion, answer: &UserInputChoice) -> bool {
    match answer {
        UserInputChoice::Option { id } => question.options.iter().any(|option| option.id == *id),
        UserInputChoice::Options { ids } => {
            question.multi_select
                && !ids.is_empty()
                && !ids
                    .iter()
                    .enumerate()
                    .any(|(index, id)| ids[..index].contains(id))
                && ids
                    .iter()
                    .all(|id| question.options.iter().any(|option| option.id == *id))
        }
        UserInputChoice::FreeText { value } => {
            question.allows_free_text && !value.trim().is_empty()
        }
    }
}

fn callback_params(
    session_id: &str,
    callback_id: &str,
    pending: &PendingCallback,
    choice: &CallbackChoice,
) -> Result<Value, ControlError> {
    let output = match choice {
        CallbackChoice::Approve {
            scope: ApprovalScope::Once,
        } => json!({"type": "approval", "decision": {"type": "approve"}}),
        CallbackChoice::Approve {
            scope: ApprovalScope::Session,
        } => json!({"type": "approval", "decision": {"type": "approve_for_session"}}),
        CallbackChoice::Approve {
            scope: ApprovalScope::Permanent,
        } => json!({"type": "approval", "decision": {"type": "approve_permanently"}}),
        CallbackChoice::Deny { .. } => {
            json!({"type": "approval", "decision": {"type": "deny"}})
        }
        CallbackChoice::Cancel if pending.kind == CallbackKind::Approval => {
            json!({"type": "approval", "decision": {"type": "cancel_turn"}})
        }
        CallbackChoice::Cancel => user_input_output(Vec::new(), true),
        CallbackChoice::Option { id } => user_input_output(
            vec![canonical_answer(
                single_question(pending)?,
                &UserInputChoice::Option { id: id.clone() },
            )?],
            false,
        ),
        CallbackChoice::Options { ids } => user_input_output(
            vec![canonical_answer(
                single_question(pending)?,
                &UserInputChoice::Options { ids: ids.clone() },
            )?],
            false,
        ),
        CallbackChoice::FreeText { value } => user_input_output(
            vec![canonical_answer(
                single_question(pending)?,
                &UserInputChoice::FreeText {
                    value: value.clone(),
                },
            )?],
            false,
        ),
        CallbackChoice::UserInput { answers } => user_input_output(
            pending
                .questions
                .iter()
                .zip(answers)
                .map(|(question, answer)| canonical_answer(question, answer))
                .collect::<Result<Vec<_>, _>>()?,
            false,
        ),
    };
    Ok(json!({
        "sessionId": session_id,
        "callbackId": callback_id,
        "output": output,
    }))
}

fn single_question(pending: &PendingCallback) -> Result<&CallbackQuestion, ControlError> {
    if pending.questions.len() == 1 {
        return Ok(&pending.questions[0]);
    }
    Err(ControlError::InvalidChoice)
}

fn canonical_answer(
    question: &CallbackQuestion,
    choice: &UserInputChoice,
) -> Result<Value, ControlError> {
    let (answer, is_other) = match choice {
        UserInputChoice::Option { id } => (
            question
                .options
                .iter()
                .find(|option| option.id == *id)
                .map(|option| option.label.clone())
                .ok_or(ControlError::InvalidChoice)?,
            false,
        ),
        UserInputChoice::Options { ids } => (
            ids.iter()
                .map(|id| {
                    question
                        .options
                        .iter()
                        .find(|option| option.id == *id)
                        .map(|option| option.label.clone())
                        .ok_or(ControlError::InvalidChoice)
                })
                .collect::<Result<Vec<_>, _>>()?
                .join(", "),
            false,
        ),
        UserInputChoice::FreeText { value } => (value.clone(), true),
    };
    Ok(json!({
        "question": question.question,
        "answer": answer,
        "isOther": is_other,
    }))
}

fn user_input_output(answers: Vec<Value>, cancelled: bool) -> Value {
    json!({
        "type": "user_input",
        "result": {
            "answers": answers,
            "cancelled": cancelled,
        }
    })
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
            kind: CallbackKind::Approval,
            prompt: "Run command?".to_owned(),
            options: Vec::new(),
            allows_free_text: false,
            multi_select: false,
            questions: Vec::new(),
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

        state.reconcile_active_callbacks(&[]);

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

        state.reconcile_active_callbacks(&[]);

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

        state.reconcile_active_callbacks(&["active"]);

        assert_eq!(
            state
                .pending_callback()
                .map(|callback| callback.callback_id.as_str()),
            Some("active")
        );
        assert_eq!(state.focus, ControlFocus::Callback);
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
                kind: CallbackKind::UserInput,
                prompt: "Questions".to_owned(),
                options: Vec::new(),
                allows_free_text: false,
                multi_select: false,
                questions,
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
