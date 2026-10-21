use serde_json::{Value, json};

use super::{
    ApprovalScope, CallbackChoice, CallbackQuestion, CallbackRequest, ControlError,
    PendingCallback, UserInputChoice,
};

pub(super) fn validate_choice(
    pending: &PendingCallback,
    choice: &CallbackChoice,
) -> Result<(), ControlError> {
    let valid = match (&pending.request, choice) {
        (
            CallbackRequest::Approval { .. },
            CallbackChoice::Approve { .. }
            | CallbackChoice::ApprovePath { .. }
            | CallbackChoice::Deny { .. }
            | CallbackChoice::Cancel,
        )
        | (
            CallbackRequest::UserInput { .. } | CallbackRequest::PlanReview { .. },
            CallbackChoice::Cancel,
        ) => true,
        (CallbackRequest::UserInput { questions, .. }, CallbackChoice::UserInput { answers }) => {
            answers.len() == questions.len()
                && answers
                    .iter()
                    .zip(questions)
                    .all(|(answer, question)| valid_question_answer(question, answer))
        }
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            choice,
        ) => {
            let question = single_question(questions)?;
            single_answer(choice).is_some_and(|answer| valid_question_answer(question, &answer))
        }
        (CallbackRequest::Approval { .. }, _) => false,
    };
    valid.then_some(()).ok_or(ControlError::InvalidChoice)
}

fn valid_question_answer(question: &CallbackQuestion, answer: &UserInputChoice) -> bool {
    match answer {
        UserInputChoice::Option { id } => option_label(question, id).is_some(),
        UserInputChoice::Options { ids } => {
            question.multi_select && !ids.is_empty() && distinct_options(question, ids)
        }
        UserInputChoice::Combined { ids, other } => {
            question.multi_select
                && question.allows_free_text
                && !other.trim().is_empty()
                && distinct_options(question, ids)
        }
        UserInputChoice::FreeText { value } => {
            question.allows_free_text && !value.trim().is_empty()
        }
    }
}

/// Whether `ids` names options of `question`, each at most once.
fn distinct_options(question: &CallbackQuestion, ids: &[String]) -> bool {
    !ids.iter()
        .enumerate()
        .any(|(index, id)| ids[..index].contains(id))
        && ids.iter().all(|id| option_label(question, id).is_some())
}

/// The label of the option of `question` that `id` names.
fn option_label<'a>(question: &'a CallbackQuestion, id: &str) -> Option<&'a str> {
    question
        .options
        .iter()
        .find(|option| option.id == id)
        .map(|option| option.label.as_str())
}

/// The answer a choice made for a lone question gives, in the shape one
/// answer of a multi-question choice takes.
fn single_answer(choice: &CallbackChoice) -> Option<UserInputChoice> {
    match choice {
        CallbackChoice::Option { id } => Some(UserInputChoice::Option { id: id.clone() }),
        CallbackChoice::Options { ids } => Some(UserInputChoice::Options { ids: ids.clone() }),
        CallbackChoice::FreeText { value } => Some(UserInputChoice::FreeText {
            value: value.clone(),
        }),
        CallbackChoice::Approve { .. }
        | CallbackChoice::ApprovePath { .. }
        | CallbackChoice::Deny { .. }
        | CallbackChoice::Cancel
        | CallbackChoice::UserInput { .. } => None,
    }
}

pub(super) fn callback_params(
    session_id: &str,
    callback_id: &str,
    pending: &PendingCallback,
    choice: &CallbackChoice,
) -> Result<Value, ControlError> {
    let output = match (&pending.request, choice) {
        (
            CallbackRequest::Approval { .. },
            CallbackChoice::Approve {
                scope: ApprovalScope::Once,
            },
        ) => json!({"type": "approval", "decision": {"type": "approve"}}),
        (
            CallbackRequest::Approval { .. },
            CallbackChoice::Approve {
                scope: ApprovalScope::Session,
            },
        ) => json!({"type": "approval", "decision": {"type": "approve_for_session"}}),
        (
            CallbackRequest::Approval { .. },
            CallbackChoice::Approve {
                scope: ApprovalScope::Permanent,
            },
        ) => json!({"type": "approval", "decision": {"type": "approve_permanently"}}),
        (CallbackRequest::Approval { .. }, CallbackChoice::ApprovePath { scope, path_scope }) => {
            json!({
                "type": "approval",
                "decision": {
                    "type": if *scope == ApprovalScope::Permanent {
                        "approve_permanently"
                    } else {
                        "approve_for_session"
                    },
                    "pathScope": path_scope,
                },
            })
        }
        (CallbackRequest::Approval { .. }, CallbackChoice::Deny { .. }) => {
            json!({"type": "approval", "decision": {"type": "deny"}})
        }
        (CallbackRequest::Approval { .. }, CallbackChoice::Cancel) => {
            json!({"type": "approval", "decision": {"type": "cancel_turn"}})
        }
        (
            CallbackRequest::UserInput { .. } | CallbackRequest::PlanReview { .. },
            CallbackChoice::Cancel,
        ) => user_input_output(Vec::new(), true),
        (CallbackRequest::UserInput { questions, .. }, CallbackChoice::UserInput { answers }) => {
            user_input_output(
                questions
                    .iter()
                    .zip(answers)
                    .map(|(question, answer)| canonical_answer(question, answer))
                    .collect::<Result<Vec<_>, _>>()?,
                false,
            )
        }
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            choice,
        ) => {
            let answer = single_answer(choice).ok_or(ControlError::InvalidChoice)?;
            user_input_output(
                vec![canonical_answer(single_question(questions)?, &answer)?],
                false,
            )
        }
        _ => return Err(ControlError::InvalidChoice),
    };
    Ok(json!({
        "sessionId": session_id,
        "callbackId": callback_id,
        "output": output,
    }))
}

fn single_question(questions: &[CallbackQuestion]) -> Result<&CallbackQuestion, ControlError> {
    if questions.len() == 1 {
        return Ok(&questions[0]);
    }
    Err(ControlError::InvalidChoice)
}

fn canonical_answer(
    question: &CallbackQuestion,
    choice: &UserInputChoice,
) -> Result<Value, ControlError> {
    let labels = |ids: &[String]| {
        ids.iter()
            .map(|id| option_label(question, id).ok_or(ControlError::InvalidChoice))
            .collect::<Result<Vec<_>, _>>()
    };
    let (answer, is_other) = match choice {
        UserInputChoice::Option { id } => (
            option_label(question, id)
                .ok_or(ControlError::InvalidChoice)?
                .to_owned(),
            false,
        ),
        UserInputChoice::Options { ids } => (labels(ids)?.join(", "), false),
        UserInputChoice::Combined { ids, other } => {
            let mut values = labels(ids)?;
            values.push(other);
            (values.join(", "), true)
        }
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
