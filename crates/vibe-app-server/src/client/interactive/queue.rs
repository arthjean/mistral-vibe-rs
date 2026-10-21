//! The questions one client is asked for its sessions, from the moment a tool
//! raises one until the client answers it.
//!
//! An in-process client and a stdio client take the same questions through the
//! same states: a turn control settles at once, a question waits while its
//! session has one open, and the rest is raised as a callback the client
//! answers. Only the delivery differs, which the caller hands in. Reference
//! `TurnController._request_approval` and `_request_user_input`
//! (`vibe/app_server/_turns.py`).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;

use serde_json::Value;
use vibe_core::events::CallbackKind as EngineCallbackKind;
use vibe_core::policy::ApprovalDecision;

use super::{
    InteractiveCallbackRequest, InteractiveCallbackResponse, approval_callback_detail,
    approval_decision_from_output, fail_interactive_response, reject_interactive_request,
    settle_turn_control,
};
use crate::client::{ClientError, MAX_INTERACTIVE_CALLBACKS, TurnDriver};
use crate::server::AppServer;

/// What a client answered to a callback the queue raised.
pub(crate) enum CallbackAnswer {
    /// The output the client gave, which the question reads.
    Output(Value),
    /// A refusal, with its reason: reference `reject_callback`, which fails the
    /// turn.
    Refused(String),
}

impl CallbackAnswer {
    /// The answer a settled callback carries: its output when the client
    /// accepted it with one, and otherwise a refusal with the text it gave.
    pub(crate) fn resolved(accepted: bool, value: Option<&str>) -> Self {
        match value
            .filter(|_| accepted)
            .map(serde_json::from_str::<Value>)
        {
            Some(Ok(output)) => Self::Output(output),
            _ => Self::Refused(value.unwrap_or_default().to_owned()),
        }
    }
}

/// A raised callback, and who waits on its answer.
struct Pending {
    session_id: String,
    turn_id: String,
    response: InteractiveCallbackResponse,
}

#[derive(Default)]
pub(crate) struct CallbackQueue {
    /// Questions waiting for their session's open callback to settle.
    backlog: VecDeque<InteractiveCallbackRequest>,
    /// The raised callbacks, by identifier.
    pending: HashMap<String, Pending>,
}

impl CallbackQueue {
    /// Raises what the backlog holds, then `incoming`, as far as each can be
    /// raised now. `deliver` puts one callback before the client, from its
    /// session, turn, kind, title and detail, and answers the identifier the
    /// server gave it with what the delivery produced; the products come back
    /// in the order the callbacks were raised.
    pub(crate) fn raise<D, T>(
        &mut self,
        server: &AppServer,
        driver: &D,
        incoming: impl IntoIterator<Item = InteractiveCallbackRequest>,
        mut deliver: impl FnMut(
            &str,
            &str,
            EngineCallbackKind,
            String,
            Value,
        ) -> Result<(String, T), String>,
    ) -> Vec<T>
    where
        D: TurnDriver + ?Sized,
    {
        let mut requests = std::mem::take(&mut self.backlog);
        requests.extend(incoming);
        let mut raised = Vec::new();
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
            if session.pending_callback.is_some()
                || self
                    .pending
                    .values()
                    .any(|pending| pending.session_id == session_id)
            {
                self.retain(request);
                continue;
            }
            let (kind, title, detail, response) = match request {
                InteractiveCallbackRequest::Approval {
                    request, response, ..
                } => {
                    let directory = PathBuf::from(&session.working_directory);
                    let remote = server
                        .tool_registry(&session_id)
                        .ok()
                        .and_then(|tools| tools.remote_origin(&request.tool));
                    (
                        EngineCallbackKind::Approval,
                        format!("Allow {}?", request.tool),
                        approval_callback_detail(&request, Some(&directory), remote.as_ref()),
                        InteractiveCallbackResponse::Approval(response),
                    )
                }
                // Reference `_request_user_input` titles every question it
                // asks the same way, whichever tool asks it.
                InteractiveCallbackRequest::Tool {
                    detail, response, ..
                } => (
                    EngineCallbackKind::UserInput,
                    "User input required".to_owned(),
                    detail,
                    InteractiveCallbackResponse::Tool(response),
                ),
                control => {
                    settle_turn_control(server, driver, control);
                    continue;
                }
            };
            match deliver(&session_id, &turn_id, kind, title, detail) {
                Ok((callback_id, _)) if self.pending.contains_key(&callback_id) => {
                    fail_interactive_response(response, "callback ID was reused");
                }
                Ok((callback_id, product)) => {
                    self.pending.insert(
                        callback_id,
                        Pending {
                            session_id,
                            turn_id,
                            response,
                        },
                    );
                    raised.push(product);
                }
                Err(error) => fail_interactive_response(response, &error),
            }
        }
        raised
    }

    fn retain(&mut self, request: InteractiveCallbackRequest) {
        if self.backlog.len() < MAX_INTERACTIVE_CALLBACKS {
            self.backlog.push_back(request);
        } else {
            reject_interactive_request(request, "interactive callback backlog is full");
        }
    }

    /// Whether `callback_id` is a callback this queue raised and still waits
    /// on.
    pub(crate) fn holds(&self, callback_id: &str) -> bool {
        self.pending.contains_key(callback_id)
    }

    /// Hands `answer` to whoever waits on `callback_id`, when the queue raised
    /// it.
    ///
    /// # Errors
    ///
    /// The output does not answer the approval it settles, which then cancels
    /// the turn.
    pub(crate) fn answer(
        &mut self,
        callback_id: &str,
        answer: CallbackAnswer,
    ) -> Result<(), ClientError> {
        let Some(pending) = self.pending.remove(callback_id) else {
            return Ok(());
        };
        match (pending.response, answer) {
            (InteractiveCallbackResponse::Approval(response), CallbackAnswer::Output(output)) => {
                match approval_decision_from_output(&output) {
                    Ok(decision) => {
                        let _ = response.send(decision);
                    }
                    Err(error) => {
                        let _ = response.send(ApprovalDecision::CancelTurn);
                        return Err(error);
                    }
                }
            }
            (InteractiveCallbackResponse::Approval(response), CallbackAnswer::Refused(message)) => {
                let _ = response.send(ApprovalDecision::Fail(message));
            }
            (InteractiveCallbackResponse::Tool(response), CallbackAnswer::Output(output)) => {
                let _ = response.send(Ok(output));
            }
            (InteractiveCallbackResponse::Tool(response), CallbackAnswer::Refused(message)) => {
                let _ = response.send(Err(message));
            }
        }
        Ok(())
    }

    /// Fails `callback_id` with `message`, when the queue raised it.
    pub(crate) fn fail_callback(&mut self, callback_id: &str, message: &str) {
        if let Some(pending) = self.pending.remove(callback_id) {
            fail_interactive_response(pending.response, message);
        }
    }

    /// Fails every question of `session_id` (every session when `None`) with
    /// `message`: the raised ones of `turn_id` (of any turn when `None`), and
    /// the waiting ones, `incoming` included. Questions of other sessions keep
    /// waiting.
    pub(crate) fn fail(
        &mut self,
        session_id: Option<&str>,
        turn_id: Option<&str>,
        message: &str,
        incoming: impl IntoIterator<Item = InteractiveCallbackRequest>,
    ) {
        let of_session = |candidate: &str| session_id.is_none_or(|expected| expected == candidate);
        let failed = self
            .pending
            .iter()
            .filter(|(_, pending)| {
                of_session(&pending.session_id)
                    && turn_id.is_none_or(|expected| expected == pending.turn_id)
            })
            .map(|(callback_id, _)| callback_id.clone())
            .collect::<Vec<_>>();
        for callback_id in failed {
            self.fail_callback(&callback_id, message);
        }
        let mut requests = std::mem::take(&mut self.backlog);
        requests.extend(incoming);
        for request in requests {
            if of_session(request.session_id()) {
                reject_interactive_request(request, message);
            } else {
                self.retain(request);
            }
        }
    }
}
