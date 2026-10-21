//! The tools and callbacks a session runs when a person is at the other end.
//!
//! An interactive client answers three things the model can ask for: an
//! approval, a set of questions ([`questions`]), and a verdict on a finished
//! plan ([`plan_review`]). Each one leaves the engine as a request on the
//! session's [`CallbackChannel`], is raised as a callback by the client's
//! [`queue::CallbackQueue`], and comes back as a tool result or a policy
//! decision. The request and answer shapes and the approval agents live here.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};

use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PathGrantScope, PolicyError,
    available_path_scopes,
};
use vibe_core::tools::{
    OwnedToolHandlerFuture, ToolError, ToolHandler, ToolInvocation, ToolOutputSink, ToolRegistry,
};

use super::ClientError;
use crate::server::{ApprovalAgentFactory, SessionToolFactory};
use plan_review::{interactive_plan_review_spec, plan_file_path, run_interactive_plan_review};
use questions::{interactive_question_spec, run_interactive_questions};
use vibe_core::events::EffectDetail;

pub(crate) mod plan_review;
pub(crate) mod questions;
pub(crate) mod queue;

pub(crate) enum InteractiveCallbackRequest {
    Approval {
        session_id: String,
        request: ApprovalRequest,
        response: tokio::sync::oneshot::Sender<ApprovalDecision>,
    },
    Tool {
        session_id: String,
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
    /// A tool asking which agent profile the session runs, which
    /// `exit_plan_mode` reads before it asks the user anything.
    ActiveAgent {
        session_id: String,
        response: tokio::sync::oneshot::Sender<Option<String>>,
    },
    /// A tool moving the session to another agent profile, which the running
    /// turn's next request already runs under (reference
    /// `InvokeContext.switch_agent_callback`).
    SwitchAgent {
        session_id: String,
        agent_name: String,
        response: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

impl InteractiveCallbackRequest {
    pub(crate) fn session_id(&self) -> &str {
        match self {
            Self::Approval { session_id, .. }
            | Self::Tool { session_id, .. }
            | Self::ClearContext { session_id, .. }
            | Self::ActiveAgent { session_id, .. }
            | Self::SwitchAgent { session_id, .. } => session_id,
        }
    }

    /// Whether the request only reads or steers the running turn, which
    /// occupies no callback slot and so never waits behind an open one.
    pub(crate) fn is_turn_control(&self) -> bool {
        matches!(
            self,
            Self::ClearContext { .. } | Self::ActiveAgent { .. } | Self::SwitchAgent { .. }
        )
    }
}

/// The channel each running turn takes its questions on, by session.
pub(crate) type TurnRoutes = Arc<
    std::sync::Mutex<
        std::collections::HashMap<
            String,
            tokio::sync::mpsc::UnboundedSender<InteractiveCallbackRequest>,
        >,
    >,
>;

/// Where an interactive tool sends what it asks the client.
#[derive(Clone)]
pub(crate) enum CallbackChannel {
    /// One queue every session of an in-process client shares.
    Shared(tokio::sync::mpsc::Sender<InteractiveCallbackRequest>),
    /// The route of the session's running turn, which a stdio client answers.
    Routed(TurnRoutes),
}

impl CallbackChannel {
    pub(crate) async fn send(&self, request: InteractiveCallbackRequest) -> Result<(), ()> {
        match self {
            Self::Shared(sender) => sender.send(request).await.map_err(drop),
            Self::Routed(routes) => {
                let route = crate::sync::lock(routes)
                    .get(request.session_id())
                    .cloned()
                    .ok_or(())?;
                route.send(request).map_err(drop)
            }
        }
    }
}

pub(crate) enum InteractiveCallbackResponse {
    Approval(tokio::sync::oneshot::Sender<ApprovalDecision>),
    Tool(tokio::sync::oneshot::Sender<Result<Value, String>>),
}

/// Approval agents that ask the client through `channel`, unless the session
/// approves every call on its own.
pub(crate) struct InteractiveApprovalFactory {
    pub(crate) channel: CallbackChannel,
}

impl ApprovalAgentFactory for InteractiveApprovalFactory {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveInteractiveRequest)
        } else {
            Arc::new(InteractiveApprovalAgent {
                session_id: session_id.to_owned(),
                channel: self.channel.clone(),
            })
        }
    }
}

pub(crate) struct ApproveInteractiveRequest;

impl ApprovalAgent for ApproveInteractiveRequest {
    fn request<'a>(&'a self, _request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async { Ok(ApprovalDecision::ApproveOnce) })
    }

    /// Reference `bypass_tool_permissions`: an auto-approving session never
    /// consults the gate at all.
    fn attribution(&self, _tool: &str) -> vibe_core::tools::ToolApprovalSource {
        vibe_core::tools::ToolApprovalSource::Bypass
    }
}

pub(crate) struct InteractiveApprovalAgent {
    pub(crate) session_id: String,
    pub(crate) channel: CallbackChannel,
}

impl ApprovalAgent for InteractiveApprovalAgent {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let (response, receiver) = tokio::sync::oneshot::channel();
            self.channel
                .send(InteractiveCallbackRequest::Approval {
                    session_id: self.session_id.clone(),
                    request,
                    response,
                })
                .await
                .map_err(|()| PolicyError::TurnCancelled)?;
            receiver.await.map_err(|_| PolicyError::TurnCancelled)
        })
    }
}

pub(crate) struct InteractiveSessionToolFactory {
    pub(crate) sender: CallbackChannel,
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
                let call_id = invocation.call_id.clone();
                Box::pin(async move {
                    run_interactive_questions(sender, session_id, call_id, arguments).await
                })
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
            move |invocation: &ToolInvocation, _output: ToolOutputSink| -> OwnedToolHandlerFuture {
                let sender = plan_sender.clone();
                let session_id = plan_session_id.clone();
                let plan_path = plan_path.clone();
                let call_id = invocation.call_id.clone();
                Box::pin(async move {
                    run_interactive_plan_review(sender, session_id, call_id, plan_path).await
                })
            },
        );
        tools
            .register(interactive_plan_review_spec(), plan_handler)
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

pub(crate) fn approval_callback_detail(
    request: &ApprovalRequest,
    working_directory: Option<&std::path::Path>,
    remote: Option<&vibe_core::events::RemoteToolOrigin>,
) -> Value {
    // The approval presents the effect it is gating, so the detail carries the
    // same typed shape the effect entry will publish once the call is allowed,
    // its paths shown against the same session directory and a proxied call
    // presented as proxied.
    let effect =
        EffectDetail::for_gated_call(&request.tool, &request.input, remote, working_directory);
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
        // Reference `ApprovalCallbackDetail.related_entry_id`: the effect the
        // approval gates, which is keyed on its tool call.
        // Reference `ApprovalCallbackDetail.path_scope_choices`: how far an
        // approval of an outside path may reach, offered by the server so the
        // client never derives a grant itself.
        "pathScopeChoices": available_path_scopes(&request.requirements),
        "relatedEntryId": request.call_id,
        // Reference `ApprovalCallbackDetail.reason` is left unset by the static
        // per-tool gate, which is the only one this port raises.
        "reason": null,
    })
}

/// Reference `get_user_cancellation_message(OPERATION_CANCELLED)`: the tag
/// is the contract, which marks the refusal as the user canceling, and the
/// sentence inside it is this port's own.
const OPERATION_CANCELLED: &str =
    "<user_cancellation>The user declined this call.</user_cancellation>";

pub(crate) fn approval_decision_from_output(
    output: &Value,
) -> Result<ApprovalDecision, ClientError> {
    if output.get("type").and_then(Value::as_str) != Some("approval") {
        return Err(ClientError::InvalidResponse(
            "approval callback returned a non-approval output".to_owned(),
        ));
    }
    // Reference `ApprovalDecision.path_scope`, checked against what was
    // offered once the policy knows the requirements it grants.
    let path_scope = match output.pointer("/decision/pathScope") {
        None | Some(Value::Null) => None,
        Some(scope) => Some(
            serde_json::from_value::<PathGrantScope>(scope.clone()).map_err(|_| {
                ClientError::InvalidResponse(format!("unknown approval path scope `{scope}`"))
            })?,
        ),
    };
    match output.pointer("/decision/type").and_then(Value::as_str) {
        Some("approve") => Ok(ApprovalDecision::ApproveOnce),
        Some("approve_for_session") => Ok(ApprovalDecision::ApproveForSession(path_scope)),
        Some("approve_permanently") => Ok(ApprovalDecision::ApprovePermanently(path_scope)),
        // Reference `_resolve_approval`: a refusal skips the call with the
        // user's reason, or with the tagged operation-cancelled notice when
        // they gave none, which ends the turn.
        Some("deny") => Ok(ApprovalDecision::Reject(
            output
                .get("feedback")
                .and_then(Value::as_str)
                .map_or_else(|| OPERATION_CANCELLED.to_owned(), str::to_owned),
        )),
        Some("cancel_turn") => Ok(ApprovalDecision::CancelTurn),
        Some(decision) => Err(ClientError::InvalidResponse(format!(
            "unknown approval decision `{decision}`"
        ))),
        None => Err(ClientError::InvalidResponse(
            "approval callback omitted its decision".to_owned(),
        )),
    }
}

/// Settles a request that reads or steers the running turn rather than
/// asking the client anything: the server answers what it knows of the
/// session, and the driver takes the control into the turn.
pub(crate) fn settle_turn_control<D: super::TurnDriver + ?Sized>(
    server: &crate::server::AppServer,
    driver: &D,
    request: InteractiveCallbackRequest,
) {
    let session_id = request.session_id().to_owned();
    let active_turn = || {
        server
            .session(&session_id)
            .map_err(|error| error.to_string())?
            .active_turn
            .ok_or_else(|| "turn is no longer active".to_owned())
    };
    match request {
        InteractiveCallbackRequest::ActiveAgent { response, .. } => {
            let agent = server
                .session(&session_id)
                .ok()
                .and_then(|session| session.intent.agent);
            let _ = response.send(agent);
        }
        InteractiveCallbackRequest::ClearContext {
            continuation,
            plan_file_path,
            response,
            ..
        } => {
            let outcome = active_turn().and_then(|turn_id| {
                driver
                    .clear_context(
                        &session_id,
                        &turn_id,
                        &continuation,
                        plan_file_path.as_deref(),
                    )
                    .map_err(|error| error.to_string())
            });
            let _ = response.send(outcome);
        }
        InteractiveCallbackRequest::SwitchAgent {
            agent_name,
            response,
            ..
        } => {
            let outcome = active_turn().and_then(|turn_id| {
                let (profile, intent, tools) = server
                    .switch_agent_in_turn(&session_id, &agent_name)
                    .map_err(|error| error.to_string())?;
                driver
                    .switch_agent(&session_id, &turn_id, &profile, &intent, &tools)
                    .map_err(|error| error.to_string())
            });
            let _ = response.send(outcome);
        }
        request => reject_interactive_request(request, "not a turn control"),
    }
}

pub(crate) fn reject_interactive_request(request: InteractiveCallbackRequest, message: &str) {
    match request {
        InteractiveCallbackRequest::Approval { response, .. } => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackRequest::Tool { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
        InteractiveCallbackRequest::ClearContext { response, .. }
        | InteractiveCallbackRequest::SwitchAgent { response, .. } => {
            let _ = response.send(Err(message.to_owned()));
        }
        InteractiveCallbackRequest::ActiveAgent { response, .. } => {
            let _ = response.send(None);
        }
    }
}

pub(crate) fn fail_interactive_response(response: InteractiveCallbackResponse, message: &str) {
    match response {
        InteractiveCallbackResponse::Approval(response) => {
            let _ = response.send(ApprovalDecision::CancelTurn);
        }
        InteractiveCallbackResponse::Tool(response) => {
            let _ = response.send(Err(message.to_owned()));
        }
    }
}

/// Reference `UserInputCallbackDetail.related_entry_id`: the effect the
/// question was raised from, when the call carried an identifier.
fn related_entry_id(call_id: String) -> Value {
    if call_id.is_empty() {
        Value::Null
    } else {
        Value::String(call_id)
    }
}

/// Asks the client the question `detail` describes and waits for its answer.
async fn request_interactive_tool_callback(
    sender: CallbackChannel,
    session_id: String,
    detail: Value,
) -> Result<Value, ToolError> {
    let (response, receiver) = tokio::sync::oneshot::channel();
    sender
        .send(InteractiveCallbackRequest::Tool {
            session_id,
            detail,
            response,
        })
        .await
        .map_err(|_| ToolError::Execution("interactive callback queue closed".to_owned()))?;
    receiver
        .await
        .map_err(|_| ToolError::Execution("interactive callback was abandoned".to_owned()))?
        .map_err(ToolError::TurnFailed)
}
