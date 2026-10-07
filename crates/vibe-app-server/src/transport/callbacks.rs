//! The approvals a stdio client answers.
//!
//! A tool that needs approval parks on its turn's channel; the turn's task
//! publishes what the turn emitted before the question, then hands the
//! question to the serve loop, which raises it as a `callback/call` to the
//! client. The client's `callback/result` settles it. Reference `TurnController._request_approval`
//! (`vibe/app_server/_turns.py`) and `_deliver_callback`
//! (`vibe/app_server/server.py`).

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use serde_json::Value;
use vibe_core::events::CallbackKind as EngineCallbackKind;
use vibe_core::policy::{
    ApprovalAgent, ApprovalDecision, ApprovalFuture, ApprovalRequest, PolicyError,
};

use crate::client::interactive::{
    ApproveInteractiveRequest, InteractiveCallbackRequest, InteractiveCallbackResponse,
    approval_callback_detail, approval_decision_from_output, fail_interactive_response,
    reject_interactive_request, settle_turn_control,
};
use crate::client::{MAX_INTERACTIVE_CALLBACKS, TurnDriver};
use crate::server::{AppServer, ApprovalAgentFactory, DeferredWork, ServerConnection};

pub(super) use crate::client::interactive::TurnRoutes;

pub(super) struct StdioApprovalFactory {
    pub(super) routes: TurnRoutes,
}

impl ApprovalAgentFactory for StdioApprovalFactory {
    fn for_session(&self, session_id: &str, auto_approve: bool) -> Arc<dyn ApprovalAgent> {
        if auto_approve {
            Arc::new(ApproveInteractiveRequest)
        } else {
            Arc::new(StdioApprovalAgent {
                session_id: session_id.to_owned(),
                routes: Arc::clone(&self.routes),
            })
        }
    }
}

struct StdioApprovalAgent {
    session_id: String,
    routes: TurnRoutes,
}

impl ApprovalAgent for StdioApprovalAgent {
    fn request<'a>(&'a self, request: ApprovalRequest) -> ApprovalFuture<'a> {
        Box::pin(async move {
            let route = self
                .routes
                .lock()
                .ok()
                .and_then(|routes| routes.get(&self.session_id).cloned())
                .ok_or(PolicyError::TurnCancelled)?;
            let (response, receiver) = tokio::sync::oneshot::channel();
            route
                .send(InteractiveCallbackRequest::Approval {
                    session_id: self.session_id.clone(),
                    request,
                    response,
                })
                .map_err(|_| PolicyError::TurnCancelled)?;
            receiver.await.map_err(|_| PolicyError::TurnCancelled)
        })
    }
}

#[derive(Default)]
pub(super) struct StdioCallbacks {
    /// Requests waiting for their session's open callback to settle.
    backlog: VecDeque<InteractiveCallbackRequest>,
    /// The raised callbacks, by identifier, and who waits on each.
    pending: HashMap<String, InteractiveCallbackResponse>,
}

impl StdioCallbacks {
    /// Raises `request`, and whatever the backlog holds that can now be
    /// raised. Answers the frames that put them on the wire.
    pub(super) fn raise<D: TurnDriver>(
        &mut self,
        connection: &mut ServerConnection,
        server: &AppServer,
        driver: &D,
        request: Option<InteractiveCallbackRequest>,
    ) -> Vec<Vec<u8>> {
        let mut requests = std::mem::take(&mut self.backlog);
        requests.extend(request);
        let mut frames = Vec::new();
        for request in requests {
            // A control occupies no callback slot: it names the running turn
            // and is settled at once, whatever callback is open.
            if request.is_turn_control() {
                settle_turn_control(server, driver, request);
                continue;
            }
            let session_id = request.session_id().to_owned();
            let Ok(session) = server.session(&session_id) else {
                reject_interactive_request(request, "session is no longer available");
                continue;
            };
            let Some(turn_id) = session.active_turn else {
                reject_interactive_request(request, "turn is no longer active");
                continue;
            };
            let (title, detail, kind) = match request {
                request if session.pending_callback.is_some() => {
                    if self.backlog.len() < MAX_INTERACTIVE_CALLBACKS {
                        self.backlog.push_back(request);
                    } else {
                        reject_interactive_request(request, "interactive callback backlog is full");
                    }
                    continue;
                }
                InteractiveCallbackRequest::Approval {
                    request: approval,
                    response,
                    ..
                } => {
                    let directory = std::path::PathBuf::from(&session.working_directory);
                    let remote = server
                        .tool_registry(&session_id)
                        .ok()
                        .and_then(|tools| tools.remote_origin(&approval.tool));
                    let detail =
                        approval_callback_detail(&approval, Some(&directory), remote.as_ref());
                    (
                        format!("Allow {}?", approval.tool),
                        detail,
                        (
                            EngineCallbackKind::Approval,
                            InteractiveCallbackResponse::Approval(response),
                        ),
                    )
                }
                // Reference `_request_user_input` titles every question it
                // asks the same way, whichever tool asks it.
                InteractiveCallbackRequest::Tool {
                    detail, response, ..
                } => (
                    "User input required".to_owned(),
                    detail,
                    (
                        EngineCallbackKind::UserInput,
                        InteractiveCallbackResponse::Tool(response),
                    ),
                ),
                request => {
                    settle_turn_control(server, driver, request);
                    continue;
                }
            };
            let (kind, response) = kind;
            match connection.request_callback_with_detail(
                &session_id,
                &turn_id,
                kind,
                title,
                detail,
            ) {
                Ok((callback_id, raised)) => {
                    self.pending.insert(callback_id, response);
                    frames.extend(raised);
                }
                Err(error) => fail_interactive_response(response, &error.to_string()),
            }
        }
        frames
    }

    /// Settles a callback this loop raised, or hands back work that is not
    /// one of them.
    pub(super) fn settle(&mut self, work: DeferredWork) -> Option<DeferredWork> {
        let DeferredWork::ResolveCallback {
            callback_id,
            accepted,
            value,
            ..
        } = &work
        else {
            return Some(work);
        };
        let Some(response) = self.pending.remove(callback_id) else {
            return Some(work);
        };
        let output = value
            .as_deref()
            .and_then(|value| serde_json::from_str::<Value>(value).ok());
        match (response, output) {
            (InteractiveCallbackResponse::Approval(response), Some(output)) if *accepted => {
                match approval_decision_from_output(&output) {
                    Ok(decision) => {
                        let _ = response.send(decision);
                    }
                    Err(error) => fail_interactive_response(
                        InteractiveCallbackResponse::Approval(response),
                        &error.to_string(),
                    ),
                }
            }
            (InteractiveCallbackResponse::Tool(response), Some(output)) if *accepted => {
                let _ = response.send(Ok(output));
            }
            // Reference `reject_callback`: a refused question fails its turn.
            (InteractiveCallbackResponse::Approval(response), _) => {
                let _ = response.send(ApprovalDecision::Fail(value.clone().unwrap_or_default()));
            }
            (response, _) => {
                fail_interactive_response(response, value.as_deref().unwrap_or_default());
            }
        }
        None
    }

    /// Fails every question still waiting, when the client goes away.
    pub(super) fn close(&mut self) {
        for request in self.backlog.drain(..) {
            reject_interactive_request(request, "the client disconnected");
        }
        for (_, response) in self.pending.drain() {
            fail_interactive_response(response, "the client disconnected");
        }
    }
}
