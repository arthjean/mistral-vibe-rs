//! Whether a tool call may run, and the callbacks that ask a person.
//!
//! Reference `_gate_tool_call` in
//! `harness/runtimes/python/python/mistralai_vibe_local_harness/vibe/_local_actions.py`
//! and `_request_approval`, `_request_user_input` and `_with_open_callbacks`
//! in `.../vibe/_session.py`: a gated call either runs, is refused with a
//! failed result, or waits on an approval callback whose answer decides it.
//! Every call carries the approval record of how it was decided. An open
//! callback is published beside the state, which shows the session blocked
//! on it, and is never journaled.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};
use tokio::sync::oneshot;

use crate::state::{HarnessState, SessionStatus, ToolAction};

pub type GateFuture<'a> = Pin<Box<dyn Future<Output = GateVerdict> + Send + 'a>>;
pub type AskFuture<'a> = Pin<Box<dyn Future<Output = CallbackAnswer> + Send + 'a>>;

/// How a callback was settled.
#[derive(Debug, Clone, PartialEq)]
pub enum CallbackAnswer {
    /// The client answered with this output.
    Output(Value),
    /// The callback was closed without an answer, for this reason.
    Closed(String),
}

/// Opens the approval callback for the call being gated.
pub trait Approver: Send + Sync {
    fn ask(&self, required_permissions: Vec<Value>, reason: Option<String>) -> AskFuture<'_>;
}

/// What the gate decided for one call.
#[derive(Debug, Clone, PartialEq)]
pub enum GateVerdict {
    /// The call runs, and its result carries this approval record.
    Run { approval: Value },
    /// The call is refused: its result fails with `tool_denied`.
    Refuse { approval: Value, message: String },
    /// The person asked to stop the turn.
    CancelTurn,
    /// The question could not be asked or answered, which fails the turn.
    FailTurn { message: String },
}

/// Decides the calls a turn makes.
pub trait ToolGate: Send + Sync {
    fn check<'a>(&'a self, action: &'a ToolAction, approver: &'a dyn Approver) -> GateFuture<'a>;
}

/// The approval record a tool result carries (reference
/// `_record_approval_meta`).
#[must_use]
pub fn approval_record(decision: &str, approval_type: &str, source: &str) -> Value {
    json!({"decision": decision, "approvalType": approval_type, "approvalSource": source})
}

/// A gate that lets every call run, recording `source` as what allowed it:
/// `bypass` for a session that skips approvals, `config` otherwise.
#[derive(Debug, Clone)]
pub struct OpenGate {
    pub source: &'static str,
}

impl ToolGate for OpenGate {
    fn check<'a>(&'a self, _action: &'a ToolAction, _approver: &'a dyn Approver) -> GateFuture<'a> {
        let approval = approval_record("execute", "always", self.source);
        Box::pin(async move { GateVerdict::Run { approval } })
    }
}

/// The callbacks a session has open, and where each answer goes.
#[derive(Debug, Default)]
pub struct CallbackBook {
    /// The open callbacks, oldest first, as harness entries.
    pub open: Vec<Value>,
    waiters: BTreeMap<String, oneshot::Sender<CallbackAnswer>>,
    /// What each settled callback was answered with, so a repeated answer
    /// is accepted once more and a different one refused.
    pub answered: BTreeMap<String, Value>,
    /// Callbacks closed without an answer.
    pub closed: Vec<String>,
    /// Answers settled but not yet handed to the call waiting on them, which
    /// the host releases once the settlement is on the wire.
    released: BTreeMap<String, (oneshot::Sender<CallbackAnswer>, CallbackAnswer)>,
}

impl CallbackBook {
    /// Opens `entry` and answers where its answer will arrive.
    pub fn open(&mut self, entry: Value) -> oneshot::Receiver<CallbackAnswer> {
        let (sender, receiver) = oneshot::channel();
        let id = entry["callbackId"].as_str().unwrap_or_default().to_owned();
        self.waiters.insert(id, sender);
        self.open.push(entry);
        receiver
    }

    /// Whether `callback_id` names a callback still open.
    #[must_use]
    pub fn is_open(&self, callback_id: &str) -> bool {
        self.open
            .iter()
            .any(|entry| entry["callbackId"] == callback_id)
    }

    /// A callback identifier not yet taken: `base`, or `base` with a random
    /// suffix when an open one already holds it.
    #[must_use]
    pub fn fresh_id(&self, base: &str) -> String {
        if self.waiters.contains_key(base) {
            format!("{base}-{}", vibe_core::session_id::random_hex::<4>())
        } else {
            base.to_owned()
        }
    }

    fn take(&mut self, callback_id: &str, state: Value, updated_at: u64) -> Option<Value> {
        let index = self
            .open
            .iter()
            .position(|entry| entry["callbackId"] == callback_id)?;
        let mut entry = self.open.remove(index);
        entry["updatedAt"] = json!(updated_at);
        entry["generationStatus"] = json!("completed");
        entry["state"] = state;
        Some(entry)
    }

    /// Settles the open callback `callback_id` with `answer`, answering its
    /// settled entry. The call waiting on it learns the answer once
    /// [`Self::release`] hands it over.
    pub fn settle(
        &mut self,
        callback_id: &str,
        answer: CallbackAnswer,
        updated_at: u64,
    ) -> Option<Value> {
        let state = match &answer {
            CallbackAnswer::Output(output) => json!({"status": "answered", "output": output}),
            CallbackAnswer::Closed(reason) => json!({"status": "cancelled", "reason": reason}),
        };
        let entry = self.take(callback_id, state, updated_at)?;
        if let Some(waiter) = self.waiters.remove(callback_id) {
            self.released
                .insert(callback_id.to_owned(), (waiter, answer));
        }
        Some(entry)
    }

    /// Hands a settled answer to the call waiting on it.
    pub fn release(&mut self, callback_id: &str) {
        if let Some((waiter, answer)) = self.released.remove(callback_id) {
            let _ = waiter.send(answer);
        }
    }

    /// Closes every open callback with `reason`, answering their settled
    /// entries. Their waiters learn the callback was closed.
    pub fn close_all(&mut self, reason: &str, updated_at: u64) -> Vec<Value> {
        let ids: Vec<String> = self
            .open
            .iter()
            .filter_map(|entry| entry["callbackId"].as_str().map(ToOwned::to_owned))
            .collect();
        let mut settled = Vec::new();
        for id in ids {
            if let Some(entry) = self.take(
                &id,
                json!({"status": "cancelled", "reason": reason}),
                updated_at,
            ) {
                if let Some(waiter) = self.waiters.remove(&id) {
                    let _ = waiter.send(CallbackAnswer::Closed(reason.to_owned()));
                }
                self.closed.push(id);
                settled.push(entry);
            }
        }
        settled
    }

    /// `state` as published while callbacks are open: the latest one blocks
    /// the session, and all of them are active (reference
    /// `_with_open_callbacks`).
    #[must_use]
    pub fn overlay(open: &[Value], state: &HarnessState) -> HarnessState {
        let mut state = state.clone();
        state.active_callbacks = open.to_vec();
        let Some(callback) = open.last() else {
            return state;
        };
        let active_turn_id = callback["turnId"]
            .as_str()
            .map(ToOwned::to_owned)
            .or_else(|| state.session.status.active_turn_id().map(ToOwned::to_owned));
        if let Some(active_turn_id) = active_turn_id {
            state.session.status = SessionStatus::Blocked {
                active_turn_id,
                callback_id: callback["callbackId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                callback_kind: callback["detail"]["kind"]
                    .as_str()
                    .unwrap_or("approval")
                    .to_owned(),
            };
        }
        state
    }
}

/// Reference `_approval_callback`: the callback asking to run `action`.
#[must_use]
pub fn approval_callback(
    session_id: &str,
    callback_id: &str,
    action: &ToolAction,
    created_at: u64,
    input_entry_id: Option<&str>,
    reason: Option<&str>,
    required_permissions: Vec<Value>,
) -> Value {
    let tool_name = action.tool_name.as_str();
    let related_entry_id = format!("effect-{}", action.action_id);
    let (message, status_text) = if tool_name == "process.start" {
        (
            "Start an interactive process. Vibe may send it further input until it exits \
             or is stopped."
                .to_owned(),
            "Waiting for approval to start an interactive process".to_owned(),
        )
    } else {
        (
            tool_name.to_owned(),
            format!("Waiting for approval to run {tool_name}"),
        )
    };
    json!({
        "type": "callback",
        "id": callback_id,
        "sessionId": session_id,
        "turnId": action.turn_id,
        "createdAt": created_at,
        "updatedAt": created_at,
        "generationStatus": "in_progress",
        "relatedEntryId": related_entry_id,
        "inputEntryId": input_entry_id,
        "callbackId": callback_id,
        "title": format!("Approve {tool_name}"),
        "detail": {
            "kind": "approval",
            "effect": {
                "kind": "tool",
                "toolName": tool_name,
                "input": action.arguments,
                "display": {
                    "summary": tool_name,
                    "content": null,
                    "suffix": "",
                    "verb": "Run",
                    "message": message,
                    "settledVerb": "Ran",
                    "settledMessage": tool_name,
                    "statusText": status_text,
                },
            },
            "requiredPermissions": required_permissions,
            "choices": ["approve", "approve_for_session", "approve_permanently", "deny", "cancel_turn"],
            "relatedEntryId": related_entry_id,
            "reason": reason,
        },
        "state": {"status": "open"},
    })
}

/// Reference `_user_input_callback`: the callback putting `action`'s
/// questions to the person.
#[must_use]
pub fn user_input_callback(
    session_id: &str,
    callback_id: &str,
    action: &ToolAction,
    call_id: &str,
    created_at: u64,
    input_entry_id: Option<&str>,
) -> Value {
    json!({
        "type": "callback",
        "id": callback_id,
        "sessionId": session_id,
        "turnId": action.turn_id,
        "createdAt": created_at,
        "updatedAt": created_at,
        "generationStatus": "in_progress",
        "relatedEntryId": call_id,
        "inputEntryId": input_entry_id,
        "callbackId": callback_id,
        "title": "User input required",
        "detail": {
            "kind": "user_input",
            "request": action.arguments,
            "relatedEntryId": call_id,
        },
        "state": {"status": "open"},
    })
}

#[cfg(test)]
mod gate_tests;
