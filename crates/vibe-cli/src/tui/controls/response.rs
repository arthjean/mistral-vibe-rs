use serde_json::{Value, json};

use super::{
    ApprovalScope, CallbackChoice, CallbackQuestion, CallbackRequest, ControlError,
    PendingCallback, UserInputChoice,
};

pub(super) fn validate_choice(
    pending: &PendingCallback,
    choice: &CallbackChoice,
) -> Result<(), ControlError> {
    match (&pending.request, choice) {
        (CallbackRequest::Approval { .. }, CallbackChoice::Approve { .. })
        | (CallbackRequest::Approval { .. }, CallbackChoice::Deny { .. })
        | (CallbackRequest::Approval { .. }, CallbackChoice::Cancel)
        | (CallbackRequest::UserInput { .. }, CallbackChoice::Cancel)
        | (CallbackRequest::PlanReview { .. }, CallbackChoice::Cancel) => Ok(()),
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::Option { id },
        ) => {
            let question = single_question(questions)?;
            question
                .options
                .iter()
                .any(|option| option.id == *id)
                .then_some(())
                .ok_or(ControlError::InvalidChoice)
        }
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::Options { ids },
        ) => {
            let question = single_question(questions)?;
            (question.multi_select
                && !ids.is_empty()
                && !ids
                    .iter()
                    .enumerate()
                    .any(|(index, id)| ids[..index].contains(id))
                && ids
                    .iter()
                    .all(|id| question.options.iter().any(|option| option.id == *id)))
            .then_some(())
            .ok_or(ControlError::InvalidChoice)
        }
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::FreeText { value },
        ) => {
            let question = single_question(questions)?;
            (question.allows_free_text && !value.trim().is_empty())
                .then_some(())
                .ok_or(ControlError::InvalidChoice)
        }
        (CallbackRequest::UserInput { questions, .. }, CallbackChoice::UserInput { answers })
            if answers.len() == questions.len()
                && answers
                    .iter()
                    .zip(questions)
                    .all(|(answer, question)| valid_question_answer(question, answer)) =>
        {
            Ok(())
        }
        _ => Err(ControlError::InvalidChoice),
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
        UserInputChoice::Combined { ids, other } => {
            question.multi_select
                && question.allows_free_text
                && !other.trim().is_empty()
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
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::Option { id },
        ) => user_input_output(
            vec![canonical_answer(
                single_question(questions)?,
                &UserInputChoice::Option { id: id.clone() },
            )?],
            false,
        ),
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::Options { ids },
        ) => user_input_output(
            vec![canonical_answer(
                single_question(questions)?,
                &UserInputChoice::Options { ids: ids.clone() },
            )?],
            false,
        ),
        (
            CallbackRequest::UserInput { questions, .. }
            | CallbackRequest::PlanReview { questions, .. },
            CallbackChoice::FreeText { value },
        ) => user_input_output(
            vec![canonical_answer(
                single_question(questions)?,
                &UserInputChoice::FreeText {
                    value: value.clone(),
                },
            )?],
            false,
        ),
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
        UserInputChoice::Combined { ids, other } => {
            let mut values = ids
                .iter()
                .map(|id| {
                    question
                        .options
                        .iter()
                        .find(|option| option.id == *id)
                        .map(|option| option.label.clone())
                        .ok_or(ControlError::InvalidChoice)
                })
                .collect::<Result<Vec<_>, _>>()?;
            values.push(other.clone());
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
