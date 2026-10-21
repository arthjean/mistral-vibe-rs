//! The `ask_user_question` tool: the questions a model puts to the user, what
//! a valid set of them is, and the result an answer becomes.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::tools::document::Document;
use vibe_core::tools::{
    ToolAvailability, ToolError, ToolExecutionOutput, ToolPresentationKind, ToolSource, ToolSpec,
};

use super::{CallbackChannel, related_entry_id, request_interactive_tool_callback};
use crate::client::{
    MAX_INTERACTIVE_OPTIONS_PER_QUESTION, MAX_INTERACTIVE_QUESTIONS, MAX_INTERACTIVE_REQUEST_BYTES,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestionRequest {
    questions: Vec<InteractiveQuestion>,
    #[serde(default)]
    footer_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestion {
    question: String,
    #[serde(default)]
    header: String,
    options: Vec<InteractiveQuestionOption>,
    #[serde(default)]
    multi_select: bool,
    #[serde(default)]
    hide_other: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InteractiveQuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}

pub(super) async fn run_interactive_questions(
    sender: CallbackChannel,
    session_id: String,
    call_id: String,
    arguments: Value,
) -> Result<ToolExecutionOutput, ToolError> {
    let request_bytes = serde_json::to_vec(&arguments)
        .map_err(|error| ToolError::Execution(format!("invalid question request: {error}")))?
        .len();
    if request_bytes > MAX_INTERACTIVE_REQUEST_BYTES {
        return Err(ToolError::Execution(format!(
            "question request exceeds {MAX_INTERACTIVE_REQUEST_BYTES} bytes"
        )));
    }
    let request = serde_json::from_value::<InteractiveQuestionRequest>(arguments)
        .map_err(|error| ToolError::Execution(format!("invalid question request: {error}")))?;
    validate_interactive_question_request(&request)?;
    let questions = request.questions.clone();
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": questions,
            "footerNote": request.footer_note.clone(),
        },
        "relatedEntryId": related_entry_id(call_id),
    });
    let output = request_interactive_tool_callback(sender, session_id, detail).await?;
    question_tool_output(&output, &request.questions)
}

fn validate_interactive_question_request(
    request: &InteractiveQuestionRequest,
) -> Result<(), ToolError> {
    if request.questions.is_empty() || request.questions.len() > MAX_INTERACTIVE_QUESTIONS {
        return Err(ToolError::Execution(format!(
            "question count must be between 1 and {MAX_INTERACTIVE_QUESTIONS}"
        )));
    }
    for question in &request.questions {
        if question.question.trim().is_empty() {
            return Err(ToolError::Execution(
                "question text must not be empty".to_owned(),
            ));
        }
        if question.header.chars().count() > 20 {
            return Err(ToolError::Execution(
                "question header exceeds 20 characters".to_owned(),
            ));
        }
        if question.options.len() < 2
            || question.options.len() > MAX_INTERACTIVE_OPTIONS_PER_QUESTION
        {
            return Err(ToolError::Execution(format!(
                "each question must have between 2 and \
                 {MAX_INTERACTIVE_OPTIONS_PER_QUESTION} options"
            )));
        }
        if question
            .options
            .iter()
            .any(|option| option.label.trim().is_empty())
        {
            return Err(ToolError::Execution(
                "question option labels must not be empty".to_owned(),
            ));
        }
    }
    Ok(())
}

fn question_tool_output(
    output: &Value,
    questions: &[InteractiveQuestion],
) -> Result<ToolExecutionOutput, ToolError> {
    let (answers, cancelled) = user_input_result(output)?;
    if cancelled {
        if !answers.is_empty() {
            return Err(ToolError::Execution(
                "cancelled question response included answers".to_owned(),
            ));
        }
        return Ok(answer_document(&[], true).into_output(json!({"kind": "user_question"})));
    }
    if answers.len() != questions.len() {
        return Err(ToolError::Execution(format!(
            "expected {} question answers, received {}",
            questions.len(),
            answers.len()
        )));
    }
    let mut published = Vec::with_capacity(answers.len());
    for (answer, question) in answers.iter().zip(questions) {
        let returned_question = required_output_string(answer, "question")?;
        let answer_text = required_output_string(answer, "answer")?;
        let is_other = answer
            .get("isOther")
            .and_then(Value::as_bool)
            .ok_or_else(|| ToolError::Execution("question answer omitted isOther".to_owned()))?;
        if returned_question != question.question {
            return Err(ToolError::Execution(
                "question answer does not match the request".to_owned(),
            ));
        }
        validate_interactive_answer(question, answer_text, is_other)?;
        published.push(PublishedAnswer {
            question: returned_question.to_owned(),
            answer: answer_text.to_owned(),
            is_other,
        });
    }
    Ok(answer_document(&published, false).into_output(json!({"kind": "user_question"})))
}

/// One answer as reference `UserAnswer` (`vibe/questions.py:58`) declares it.
///
/// The client answers over the wire in the camel case the request model
/// aliases, and the result the model reads is the field names themselves, so
/// the two spellings are separated here rather than passed through.
struct PublishedAnswer {
    question: String,
    answer: String,
    is_other: bool,
}

/// Reference `UserQuestionResult` (`vibe/questions.py:66`) declares `answers`
/// and `cancelled`, and the agent loop renders one field per line, with the
/// answer list as Python's repr of a list of dictionaries.
fn answer_document(answers: &[PublishedAnswer], cancelled: bool) -> Document {
    let answers = answers
        .iter()
        .map(|answer| {
            Document::new()
                .field("question", answer.question.as_str())
                .field("answer", answer.answer.as_str())
                .field("is_other", answer.is_other)
        })
        .collect::<Vec<_>>();
    Document::new()
        .field("answers", answers)
        .field("cancelled", cancelled)
}

fn validate_interactive_answer(
    question: &InteractiveQuestion,
    answer: &str,
    is_other: bool,
) -> Result<(), ToolError> {
    if answer.trim().is_empty() {
        return Err(ToolError::Execution(
            "question answer must not be empty".to_owned(),
        ));
    }
    if is_other {
        return if question.hide_other {
            Err(ToolError::Execution(
                "question does not allow a free-text answer".to_owned(),
            ))
        } else {
            Ok(())
        };
    }
    let labels = answer.split(", ").collect::<Vec<_>>();
    if (!question.multi_select && labels.len() != 1)
        || labels
            .iter()
            .any(|label| !question.options.iter().any(|option| option.label == *label))
        || labels
            .iter()
            .enumerate()
            .any(|(index, label)| labels[..index].contains(label))
    {
        return Err(ToolError::Execution(
            "question answer selected an invalid option".to_owned(),
        ));
    }
    Ok(())
}

pub(super) fn user_input_result(output: &Value) -> Result<(&[Value], bool), ToolError> {
    if output.get("type").and_then(Value::as_str) != Some("user_input") {
        return Err(ToolError::Execution(
            "interactive tool returned a non-user-input output".to_owned(),
        ));
    }
    let result = output
        .get("result")
        .and_then(Value::as_object)
        .ok_or_else(|| ToolError::Execution("user-input output omitted result".to_owned()))?;
    let answers = result
        .get("answers")
        .and_then(Value::as_array)
        .ok_or_else(|| ToolError::Execution("user-input output omitted answers".to_owned()))?;
    let cancelled = result
        .get("cancelled")
        .and_then(Value::as_bool)
        .ok_or_else(|| ToolError::Execution("user-input output omitted cancelled".to_owned()))?;
    Ok((answers, cancelled))
}

fn required_output_string<'a>(value: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Execution(format!("interactive answer omitted string `{key}`")))
}

/// Directive coverage for `ask_user_question`, whose reference description
/// this port must cover without reproducing (`NOTICE`).
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The question carries structured options rather than free prose | "structured options" in the description |
/// | One to four questions per call | "one to four questions" |
/// | Each question has a header, a question text, and its options | the property descriptions plus `minItems` |
/// | Two to four options per question | "two to four options", `minItems: 2` |
/// | A free-text "Other" option is appended by the client | "an Other free-text choice is appended" |
///
/// The argument shape comes from the reference `UserQuestionRequest`, which is
/// the one reference model configuring `alias_generator=to_camel` alongside
/// `extra="forbid"`: its properties are camelCase where every other reference
/// tool stays snake_case.
pub(crate) fn interactive_question_spec() -> ToolSpec {
    let choice = ObjectSchema::new()
        .required(
            "label",
            Property::string().described("A one to five word label for the choice"),
        )
        .optional(
            "description",
            Property::string()
                .described("An optional expansion of what the choice means")
                .with_default(""),
        )
        .forbid_extra_properties();
    let question = ObjectSchema::new()
        .required(
            "question",
            Property::string().described("The text of the question"),
        )
        .required(
            "options",
            Property::array(Property::reference("QuestionChoice"))
                .constrained("minItems", 2)
                .described(
                    "The choices offered, two to four of them, not counting the Other free-text \
                     choice the client appends.",
                ),
        )
        .optional(
            "header",
            Property::string()
                .constrained("maxLength", 20)
                .described("A short chip label for the question, at most 20 characters")
                .with_default(""),
        )
        .optional(
            "hideOther",
            Property::boolean()
                .described("When true, the Other free-text choice is withheld")
                .with_default(false),
        )
        .optional(
            "multiSelect",
            Property::boolean()
                .described("When true, several choices may be selected at once")
                .with_default(false),
        )
        .forbid_extra_properties();
    ToolSpec {
        name: "ask_user_question".to_owned(),
        description: "Put a question to the user with structured options. Send one to four \
                      questions, each carrying a header, its question text, and two to four \
                      options; an Other free-text choice is appended for you."
            .to_owned(),
        input_schema: ObjectSchema::new()
            .define("QuestionChoice", choice)
            .define("UserQuestion", question)
            .required(
                "questions",
                Property::array(Property::reference("UserQuestion"))
                    .constrained("minItems", 1)
                    .described(
                        "The questions to put, one to four of them. Several questions render as \
                         tabs.",
                    ),
            )
            .optional(
                "footerNote",
                Property::string()
                    .described("An optional quiet note rendered under the question widget.")
                    .with_default(Value::Null)
                    .nullable(),
            )
            .forbid_extra_properties()
            .build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 50,
    }
}
