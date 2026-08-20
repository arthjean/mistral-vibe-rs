//! The tools and callbacks a session runs when a person is at the other end.
//!
//! An interactive client answers three things the model can ask for: an
//! approval, a set of questions, and a verdict on a finished plan. Each one
//! leaves the engine as a callback request, waits for the client's answer, and
//! comes back as a tool result. The request and answer shapes, the validation
//! that a body is one of them, and the two tool specifications that publish
//! them all live here.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PolicyError,
};
use vibe_core::schema::{ObjectSchema, Property};
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolAvailability, ToolError, ToolExecutionOutput, ToolHandler,
    ToolInvocation, ToolOutputSink, ToolPresentationKind, ToolRegistry, ToolSource, ToolSpec,
    reference_text,
};

use super::{
    ClientError, MAX_INTERACTIVE_CALLBACKS, MAX_INTERACTIVE_OPTIONS_PER_QUESTION,
    MAX_INTERACTIVE_QUESTIONS, MAX_INTERACTIVE_REQUEST_BYTES,
};
use crate::server::{ApprovalAgentFactory, SessionToolFactory};
use vibe_core::events::EffectDetail;

pub(crate) enum InteractiveCallbackRequest {
    Approval {
        session_id: String,
        request: ApprovalRequest,
        response: tokio::sync::oneshot::Sender<ApprovalDecision>,
    },
    Tool {
        session_id: String,
        title: String,
        detail: Value,
        response: tokio::sync::oneshot::Sender<Result<Value, String>>,
    },
    /// A tool asking for the transcript to be dropped and the session rotated
    /// at the running turn's next cycle boundary.
    ///
    /// It crosses the same channel as a callback because it needs the same
    /// thing a callback does: the identifier of the turn the server reserved,
    /// which a tool handler knows nothing about.
    ClearContext {
        session_id: String,
        continuation: String,
        plan_file_path: Option<String>,
        response: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

pub(super) enum InteractiveCallbackResponse {
    Approval(tokio::sync::oneshot::Sender<ApprovalDecision>),
    Tool(tokio::sync::oneshot::Sender<Result<Value, String>>),
}

pub(super) struct PendingInteractiveCallback {
    pub(super) session_id: String,
    pub(super) turn_id: String,
    pub(super) response: InteractiveCallbackResponse,
}

pub(super) struct InteractiveApprovalFactory {
    pub(super) sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
}

impl ApprovalAgentFactory for InteractiveApprovalFactory {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveInteractiveRequest)
        } else {
            Arc::new(InteractiveApprovalAgent {
                session_id: session_id.to_owned(),
                sender: self.sender.clone(),
            })
        }
    }
}

pub(super) struct ApproveInteractiveRequest;

impl ApprovalAgent for ApproveInteractiveRequest {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }
}

pub(super) struct InteractiveApprovalAgent {
    pub(super) session_id: String,
    pub(super) sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
}

impl ApprovalAgent for InteractiveApprovalAgent {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let (response, receiver) = tokio::sync::oneshot::channel();
            self.sender
                .send(InteractiveCallbackRequest::Approval {
                    session_id: self.session_id.clone(),
                    request,
                    response,
                })
                .await
                .map_err(|_| PolicyError::TurnCancelled)?;
            receiver.await.map_err(|_| PolicyError::TurnCancelled)
        })
    }
}

pub(crate) struct InteractiveSessionToolFactory {
    pub(crate) sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    pub(crate) plan_directory: Option<PathBuf>,
}

impl SessionToolFactory for InteractiveSessionToolFactory {
    fn register(&self, session_id: &str, tools: &ToolRegistry) -> Result<(), String> {
        let question_sender = self.sender.clone();
        let question_session_id = session_id.to_owned();
        let question_handler: Arc<dyn ToolHandler> = Arc::new(
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let sender = question_sender.clone();
                let session_id = question_session_id.clone();
                let arguments = invocation.arguments.clone();
                Box::pin(
                    async move { run_interactive_questions(sender, session_id, arguments).await },
                )
            },
        );
        tools
            .register(interactive_question_spec(), question_handler)
            .map_err(|error| error.to_string())?;

        let Some(plan_directory) = self.plan_directory.as_deref() else {
            return Ok(());
        };
        let plan_sender = self.sender.clone();
        let plan_session_id = session_id.to_owned();
        let plan_path = plan_file_path(plan_directory, session_id);
        let plan_handler: Arc<dyn ToolHandler> = Arc::new(
            move |_invocation: &ToolInvocation,
                  _output: ToolOutputSink|
                  -> OwnedToolHandlerFuture {
                let sender = plan_sender.clone();
                let session_id = plan_session_id.clone();
                let plan_path = plan_path.clone();
                Box::pin(
                    async move { run_interactive_plan_review(sender, session_id, plan_path).await },
                )
            },
        );
        tools
            .register(interactive_plan_review_spec(), plan_handler)
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InteractiveQuestionRequest {
    questions: Vec<InteractiveQuestion>,
    #[serde(default)]
    footer_note: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(super) struct InteractiveQuestion {
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
pub(super) struct InteractiveQuestionOption {
    label: String,
    #[serde(default)]
    description: String,
}
pub(super) fn approval_callback_detail(request: &ApprovalRequest) -> Value {
    // The approval presents the effect it is gating, so the detail carries the
    // same typed shape the effect entry will publish once the call is allowed.
    let mut effect = EffectDetail::for_call(&request.tool, &request.input);
    effect.display.content = Some(request.rationale.clone());
    json!({
        "kind": "approval",
        "effect": effect,
        // Reference `ApprovalCallbackDetail.required_permissions` is a list of
        // the requirement model itself, not of its labels: the client needs the
        // scope and the two patterns to render what an "allow always" would
        // grant.
        "requiredPermissions": request.requirements,
        "choices": [
            "approve",
            "approve_for_session",
            "approve_permanently",
            "deny",
            "cancel_turn",
        ],
        "relatedEntryId": null,
    })
}

pub(super) fn approval_decision_from_output(
    output: &Value,
) -> Result<ApprovalDecision, ClientError> {
    if output.get("type").and_then(Value::as_str) != Some("approval") {
        return Err(ClientError::InvalidResponse(
            "approval callback returned a non-approval output".to_owned(),
        ));
    }
    match output.pointer("/decision/type").and_then(Value::as_str) {
        Some("approve") => Ok(ApprovalDecision::ApproveOnce),
        Some("approve_for_session") => Ok(ApprovalDecision::ApproveForSession),
        Some("approve_permanently") => Ok(ApprovalDecision::ApprovePermanently),
        Some("deny") => Ok(ApprovalDecision::Deny),
        Some("cancel_turn") => Ok(ApprovalDecision::CancelTurn),
        Some(decision) => Err(ClientError::InvalidResponse(format!(
            "unknown approval decision `{decision}`"
        ))),
        None => Err(ClientError::InvalidResponse(
            "approval callback omitted its decision".to_owned(),
        )),
    }
}

pub(super) fn interactive_request_session_id(request: &InteractiveCallbackRequest) -> &str {
    match request {
        InteractiveCallbackRequest::Approval { session_id, .. }
        | InteractiveCallbackRequest::Tool { session_id, .. }
        | InteractiveCallbackRequest::ClearContext { session_id, .. } => session_id,
    }
}

pub(super) fn reject_interactive_request(request: InteractiveCallbackRequest, message: &str) {
    match request {
        InteractiveCallbackRequest::Approval { response, .. } => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackRequest::Tool { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
        InteractiveCallbackRequest::ClearContext { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

pub(super) fn retain_interactive_request(
    backlog: &mut VecDeque<InteractiveCallbackRequest>,
    request: InteractiveCallbackRequest,
) {
    if backlog.len() < MAX_INTERACTIVE_CALLBACKS {
        backlog.push_back(request);
    } else {
        reject_interactive_request(request, "interactive callback backlog is full");
    }
}

pub(super) fn fail_pending_interactive_callback(
    pending: Option<PendingInteractiveCallback>,
    message: &str,
) {
    if let Some(pending) = pending {
        fail_interactive_response(pending.response, message);
    }
}

pub(super) fn fail_interactive_response(response: InteractiveCallbackResponse, message: &str) {
    match response {
        InteractiveCallbackResponse::Approval(response) => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackResponse::Tool(response) => {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

pub(super) async fn run_interactive_questions(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
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
    let title = request
        .questions
        .iter()
        .enumerate()
        .map(|(index, question)| {
            if question.header.is_empty() {
                format!("{}. {}", index + 1, question.question)
            } else {
                format!("{}. {}: {}", index + 1, question.header, question.question)
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": questions,
            "footerNote": request.footer_note.clone(),
        },
        "relatedEntryId": null,
    });
    let output = request_interactive_tool_callback(sender, session_id, title, detail).await?;
    question_tool_output(&output, &request.questions)
}

pub(super) fn validate_interactive_question_request(
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

pub(super) async fn request_interactive_tool_callback(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    title: String,
    detail: Value,
) -> Result<Value, ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::Tool {
            session_id,
            title,
            detail,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("interactive callback was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

/// Asks the surface driving the turn to clear the transcript and rotate the
/// session, and waits for it to be queued.
///
/// Waiting is what makes the answer honest: the tool reports a cleared context
/// only once the turn actually holds the control that clears it.
pub(super) async fn request_context_clearing(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    continuation: String,
    plan_file_path: Option<String>,
) -> Result<(), ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::ClearContext {
            session_id,
            continuation,
            plan_file_path,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("context clearing was abandoned".to_owned()))?
        .map_err(ToolError::Execution)
}

pub(super) fn question_tool_output(
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
        return Ok(ToolExecutionOutput {
            typed_result: json!({"answers": [], "cancelled": true}),
            model_text: answer_model_text(&[], true),
            display: json!({"kind": "user_question"}),
            projected_result: serde_json::Value::Null,
            chunks: Vec::new(),
        });
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
    Ok(ToolExecutionOutput {
        typed_result: json!({
            "answers": published
                .iter()
                .map(|answer| json!({
                    "question": answer.question,
                    "answer": answer.answer,
                    "is_other": answer.is_other,
                }))
                .collect::<Vec<_>>(),
            "cancelled": false,
        }),
        model_text: answer_model_text(&published, false),
        display: json!({"kind": "user_question"}),
        projected_result: serde_json::Value::Null,
        chunks: Vec::new(),
    })
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
fn answer_model_text(answers: &[PublishedAnswer], cancelled: bool) -> String {
    let rendered = answers
        .iter()
        .map(|answer| {
            vec![
                ("question", reference_text::string_repr(&answer.question)),
                ("answer", reference_text::string_repr(&answer.answer)),
                (
                    "is_other",
                    reference_text::boolean(answer.is_other).to_owned(),
                ),
            ]
        })
        .collect::<Vec<_>>();
    reference_text::joined(&[
        ("answers", reference_text::dictionary_list(&rendered)),
        ("cancelled", reference_text::boolean(cancelled).to_owned()),
    ])
}

pub(super) fn validate_interactive_answer(
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

pub(super) async fn run_interactive_plan_review(
    sender: tokio::sync::mpsc::Sender<InteractiveCallbackRequest>,
    session_id: String,
    plan_path: PathBuf,
) -> Result<ToolExecutionOutput, ToolError> {
    const QUESTION: &str = "Plan is complete. Switch to code mode and start implementing?";
    let detail = json!({
        "kind": "user_input",
        "request": {
            "questions": [{
                "question": QUESTION,
                "header": "Plan ready",
                "options": interactive_plan_options(),
                "multiSelect": false,
                "hideOther": false,
            }],
            "footerNote": null,
        },
        "planReview": true,
        "filePath": plan_path,
        "relatedEntryId": null,
    });
    let output = request_interactive_tool_callback(
        sender.clone(),
        session_id.clone(),
        QUESTION.to_owned(),
        detail,
    )
    .await?;
    let (answers, cancelled) = user_input_result(&output)?;
    let (switched, clear_context, message) = if cancelled {
        if !answers.is_empty() {
            return Err(ToolError::Execution(
                "cancelled plan review included an answer".to_owned(),
            ));
        }
        (
            false,
            false,
            "User cancelled. Staying in plan mode.".to_owned(),
        )
    } else {
        if answers.len() != 1 {
            return Err(ToolError::Execution(
                "plan review requires exactly one answer".to_owned(),
            ));
        }
        let answer = &answers[0];
        if required_output_string(answer, "question")? != QUESTION {
            return Err(ToolError::Execution(
                "plan review answer does not match the request".to_owned(),
            ));
        }
        let value = required_output_string(answer, "answer")?;
        if answer.get("isOther").and_then(Value::as_bool) == Some(true) {
            if value.trim().is_empty() {
                return Err(ToolError::Execution(
                    "plan feedback must not be empty".to_owned(),
                ));
            }
            (
                false,
                false,
                format!("Stay in plan mode and incorporate this feedback: {value}"),
            )
        } else {
            match value {
                "Yes, clear context and auto approve edits" => (
                    true,
                    true,
                    "Plan approved. Switch to code mode, clear planning context, and auto approve \
                     edits."
                        .to_owned(),
                ),
                "Yes, and auto approve edits" => (
                    true,
                    false,
                    "Plan approved. Switch to code mode and auto approve edits.".to_owned(),
                ),
                "Yes, and request approval for edits" => (
                    true,
                    false,
                    "Plan approved. Switch to code mode and request approval for edits.".to_owned(),
                ),
                "No" => (
                    false,
                    false,
                    "Plan rejected. Stay in plan mode and continue refining it.".to_owned(),
                ),
                _ => {
                    return Err(ToolError::Execution(
                        "plan review selected an invalid option".to_owned(),
                    ));
                }
            }
        }
    };
    if clear_context {
        // The clearing lands at the next cycle boundary, so the message this
        // tool answers with is also the only instruction that survives it.
        request_context_clearing(
            sender,
            session_id,
            message.clone(),
            plan_path.to_str().map(str::to_owned),
        )
        .await?;
    }
    // Reference `ExitPlanModeResult`
    // (`vibe/core/tools/builtins/exit_plan_mode.py:34`) declares `switched` then
    // `message`, and the agent loop renders one field per line, so the decision
    // reaches the model beside the sentence rather than only inside it.
    let model_text = reference_text::joined(&[
        ("switched", reference_text::boolean(switched).to_owned()),
        ("message", message.clone()),
    ]);
    Ok(ToolExecutionOutput {
        typed_result: json!({"switched": switched, "message": message}),
        model_text,
        display: json!({"kind": "plan_review", "switched": switched}),
        projected_result: serde_json::Value::Null,
        chunks: Vec::new(),
    })
}

pub(super) fn plan_file_path(plan_directory: &Path, session_id: &str) -> PathBuf {
    let safe_session = session_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    plan_directory.join(format!("{safe_session}.md"))
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

pub(super) fn required_output_string<'a>(
    value: &'a Value,
    key: &str,
) -> Result<&'a str, ToolError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolError::Execution(format!("interactive answer omitted string `{key}`")))
}

pub(super) fn interactive_plan_options() -> Vec<Value> {
    [
        (
            "Yes, clear context and auto approve edits",
            "Clear planning context, switch to code mode, and auto approve edits",
        ),
        (
            "Yes, and auto approve edits",
            "Switch to code mode with auto-approved edits",
        ),
        (
            "Yes, and request approval for edits",
            "Switch to code mode and keep edit approvals",
        ),
        ("No", "Stay in plan mode and continue planning"),
    ]
    .into_iter()
    .map(|(label, description)| json!({"label": label, "description": description}))
    .collect()
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
pub(super) fn interactive_question_spec() -> ToolSpec {
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

/// Directive coverage for `exit_plan_mode`.
///
/// | Reference directive | Covered by |
/// |---|---|
/// | The call announces a finished plan ready to implement | "the plan is finished and ready to implement" |
/// | Call it only once the plan is final | "Call it only once the plan is final" |
/// | Do not call it while planning continues | "never while planning is still under way or the user wants to keep planning" |
pub(super) fn interactive_plan_review_spec() -> ToolSpec {
    ToolSpec {
        name: "exit_plan_mode".to_owned(),
        description: "Announce that the plan is finished and ready to implement. Call it only \
                      once the plan is final, never while planning is still under way or the \
                      user wants to keep planning."
            .to_owned(),
        input_schema: ObjectSchema::new().build(),
        output_schema: None,
        config: Value::Null,
        state: Value::Null,
        availability: ToolAvailability::Available,
        presentation: ToolPresentationKind::Generic,
        source: ToolSource::BuiltIn,
        selection_priority: 50,
    }
}
