//! The callbacks a turn raises, answered by asking the client.
//!
//! Reference `VibeAcpAgent._answer_callback` and `_answer_user_input`: an
//! approval becomes a permission request, a question becomes a form
//! elicitation, and anything the client cannot answer is denied.

use std::sync::Arc;

use serde_json::{Map, Value, json};
use vibe_app_server::client::{
    CallbackDetail, PublicCallbackState, PublicHistoryEntry, TurnDriver,
};
use vibe_core::events::UserQuestionRequest;

use crate::agent::AcpAgent;
use crate::projection::updated_entry_updates;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

/// The feedback a rejected approval hands the model.
const REJECTION_FEEDBACK: &str = "The user turned this tool call down; suggest another approach.";

impl<D> AcpAgent<D>
where
    D: TurnDriver + 'static,
{
    pub(super) async fn route_pending_callbacks(
        &self,
        harness: &Arc<AcpHarness<D>>,
    ) -> Result<(), AcpError> {
        let callbacks = harness.service.lock().await.drain_callbacks()?;
        for callback in callbacks {
            let PublicHistoryEntry::Callback {
                metadata,
                callback_id,
                detail,
                state: PublicCallbackState::Open,
                ..
            } = callback
            else {
                continue;
            };
            // Reference `EventProjector.open_callback`: the effect waiting on
            // the answer is blocked until it comes, then runs again.
            let effect = detail.related_entry_id().map(|related| {
                (
                    metadata.turn_id.clone().unwrap_or_default(),
                    related.to_owned(),
                )
            });
            if let Some((turn, related)) = &effect {
                self.mark_effect(harness, turn, related, Some(&callback_id));
            }
            let output = match detail {
                CallbackDetail::Approval {
                    effect,
                    required_permissions,
                    related_entry_id,
                    ..
                } => {
                    let tool_call_id = related_entry_id.unwrap_or(effect.tool_name);
                    let permissions = required_permissions
                        .iter()
                        .map(|permission| {
                            json!({
                                "scope": permission.scope,
                                "invocation_pattern": permission.invocation_pattern,
                                "session_pattern": permission.session_pattern,
                                "label": permission.label,
                            })
                        })
                        .collect::<Vec<_>>();
                    self.answer_approval(harness, &tool_call_id, permissions)
                        .await?
                }
                CallbackDetail::UserInput {
                    request,
                    related_entry_id,
                } => {
                    match self
                        .answer_user_input(harness, &request, related_entry_id)
                        .await?
                    {
                        Ok(output) => output,
                        Err(message) => {
                            harness.service.lock().await.reject_callback(
                                json!({
                                    "sessionId": harness.canonical_id(),
                                    "callbackId": callback_id,
                                    "output": denied_question(),
                                }),
                                &message,
                            )?;
                            // A refusal fails the turn, which settles the
                            // effect; it never runs again.
                            continue;
                        }
                    }
                }
            };
            harness.service.lock().await.respond_callback(json!({
                "sessionId": harness.canonical_id(),
                "callbackId": callback_id,
                "output": output,
            }))?;
            if let Some((turn, related)) = &effect {
                self.mark_effect(harness, turn, related, None);
            }
        }
        Ok(())
    }

    /// Moves the effect `related` of `turn` to blocked on `callback`, or back
    /// to running, and tells the client what changed.
    fn mark_effect(
        &self,
        harness: &AcpHarness<D>,
        turn: &str,
        related: &str,
        callback: Option<&str>,
    ) {
        let Some(previous) = harness.remembered_entry(turn, related) else {
            return;
        };
        let status = previous.pointer("/state/status").and_then(Value::as_str);
        if !matches!(status, Some("running" | "blocked")) {
            return;
        }
        let output_text = previous
            .pointer("/state/outputText")
            .cloned()
            .unwrap_or_else(|| json!(""));
        let mut entry = previous.clone();
        entry["state"] = match callback {
            Some(callback_id) => json!({
                "status": "blocked",
                "callbackId": callback_id,
                "outputText": output_text,
            }),
            None => json!({"status": "running", "outputText": output_text}),
        };
        harness.remember_entry(&entry);
        for update in updated_entry_updates(&previous, &entry) {
            self.session_update(&harness.session_id, update);
        }
    }

    /// Reference `_answer_callback` for an approval.
    async fn answer_approval(
        &self,
        harness: &Arc<AcpHarness<D>>,
        tool_call_id: &str,
        permissions: Vec<Value>,
    ) -> Result<Value, AcpError> {
        let session_meta =
            (!permissions.is_empty()).then(|| json!({"required_permissions": permissions}));
        let mut always = json!({
            "optionId": "allow_always",
            "name": "Allow for the rest of this session",
            "kind": "allow_always",
        });
        let mut permanent = json!({
            "optionId": "allow_always_permanent",
            "name": "Always allow",
            "kind": "allow_always",
        });
        if let Some(meta) = &session_meta {
            always["_meta"] = meta.clone();
            permanent["_meta"] = meta.clone();
        }
        let options = json!([
            {"optionId": "allow_once", "name": "Allow once", "kind": "allow_once"},
            always,
            permanent,
            {"optionId": "reject_once", "name": "Deny", "kind": "reject_once"},
        ]);
        let request = self.call_client(
            "session/request_permission",
            json!({
                "sessionId": harness.session_id,
                "toolCall": {"toolCallId": tool_call_id},
                "options": options,
            }),
        );
        let response = tokio::select! {
            response = request => Some(response?),
            () = harness.cancelled() => None,
        };
        let outcome = response
            .as_ref()
            .and_then(|response| response.get("outcome"));
        let selected = outcome
            .filter(|outcome| outcome.get("outcome").and_then(Value::as_str) == Some("selected"))
            .and_then(|outcome| outcome.get("optionId"))
            .and_then(Value::as_str);
        let (decision, feedback) = match selected {
            Some("allow_once") => ("approve", None),
            Some("allow_always") => ("approve_for_session", None),
            Some("allow_always_permanent") => ("approve_permanently", None),
            Some("reject_once") => {
                self.record(
                    harness,
                    "vibe.user_cancelled_action",
                    json!({"action": "reject_approval"}),
                )
                .await;
                ("deny", Some(REJECTION_FEEDBACK))
            }
            _ => ("deny", None),
        };
        let mut output = json!({"type": "approval", "decision": {"type": decision}});
        if let Some(feedback) = feedback {
            output["feedback"] = json!(feedback);
        }
        Ok(output)
    }

    /// Reference `_answer_user_input`: the questions as a form, and the
    /// answers read back. The inner error is a reply the client gave that
    /// does not answer the form.
    async fn answer_user_input(
        &self,
        harness: &Arc<AcpHarness<D>>,
        request: &UserQuestionRequest,
        related_entry_id: Option<String>,
    ) -> Result<Result<Value, String>, AcpError> {
        let schema = elicitation_schema(request);
        let mut mode = json!({
            "mode": "form",
            "sessionId": harness.session_id,
            "requestedSchema": schema,
        });
        if let Some(tool_call_id) = related_entry_id {
            mode["toolCallId"] = json!(tool_call_id);
        }
        let mut params = mode;
        params["message"] = json!("Your input is needed");
        let request_future = self.call_client("elicitation/create", params);
        let response = tokio::select! {
            response = request_future => Some(response?),
            () = harness.cancelled() => None,
        };
        let content = response.as_ref().and_then(|response| {
            (response.get("action").and_then(Value::as_str) == Some("accept"))
                .then(|| response.get("content"))
                .flatten()
                .and_then(Value::as_object)
                .filter(|content| !content.is_empty())
        });
        let Some(content) = content else {
            return Ok(Ok(denied_question()));
        };
        Ok(user_answers(request, content).map(|answers| {
            json!({
                "type": "user_input",
                "result": {"answers": answers, "cancelled": false},
            })
        }))
    }
}

fn denied_question() -> Value {
    json!({"type": "user_input", "result": {"answers": [], "cancelled": true}})
}

/// Reference `_build_elicitation_schema`: one property per question, a
/// single choice as a string and a multiple choice as an array.
fn elicitation_schema(request: &UserQuestionRequest) -> Value {
    let mut properties = Map::new();
    for (index, question) in request.questions.iter().enumerate() {
        let options = question
            .options
            .iter()
            .map(|option| json!({"const": option.label, "title": option.label}))
            .collect::<Vec<_>>();
        let title = Some(question.header.as_str())
            .filter(|header| !header.is_empty())
            .map_or(Value::Null, |header| json!(header));
        let mut property = if question.multi_select {
            json!({
                "type": "array",
                "description": question.question,
                "items": {"anyOf": options},
            })
        } else {
            json!({
                "type": "string",
                "description": question.question,
                "oneOf": options,
            })
        };
        if !title.is_null() {
            property["title"] = title;
        }
        properties.insert(format!("q{index}"), property);
    }
    let required = properties.keys().cloned().collect::<Vec<_>>();
    // The reference's `ElicitationSchema` leaves its `type` default unset, so
    // the wire object carries no `type` key.
    let mut schema = json!({
        "properties": properties,
        "required": required,
    });
    if let Some(footer) = &request.footer_note {
        schema["description"] = json!(footer);
    }
    schema
}

/// Reference `_elicit_user_answers`.
fn user_answers(
    request: &UserQuestionRequest,
    content: &Map<String, Value>,
) -> Result<Vec<Value>, String> {
    let mut answers = Vec::new();
    for (index, question) in request.questions.iter().enumerate() {
        let key = format!("q{index}");
        let labels = question
            .options
            .iter()
            .map(|option| option.label.as_str())
            .collect::<Vec<_>>();
        let raw = content.get(&key);
        if question.multi_select {
            let Some(items) = raw.and_then(Value::as_array) else {
                return Err(format!("the answer to {key} must be a list"));
            };
            if items.is_empty() {
                return Err(format!("the answer to {key} is an empty list"));
            }
            let rendered = items.iter().map(python_str).collect::<Vec<_>>();
            let is_other = rendered.iter().any(|item| !labels.contains(&item.as_str()));
            answers.push(json!({
                "question": question.question,
                "answer": rendered.join(", "),
                "isOther": is_other,
            }));
        } else {
            let Some(answer) = raw.and_then(Value::as_str) else {
                return Err(format!("the answer to {key} must be text"));
            };
            if answer.is_empty() {
                return Err(format!("the answer to {key} is empty"));
            }
            answers.push(json!({
                "question": question.question,
                "answer": answer,
                "isOther": !labels.contains(&answer),
            }));
        }
    }
    Ok(answers)
}

/// Python's `str()` of a JSON value, as a joined answer renders it.
fn python_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Null => "None".to_owned(),
        other => other.to_string(),
    }
}
