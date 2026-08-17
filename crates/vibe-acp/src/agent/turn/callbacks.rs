//! Canonical callbacks raised mid-turn, and the ACP permission request an
//! approval becomes.
//!
//! ACP v1 has no interactive question flow, so only approvals reach the
//! client; anything else is declined without being surfaced.

use std::sync::Arc;

use serde_json::{Value, json};
use vibe_app_server::client::{
    CallbackDetail, PublicCallbackState, PublicHistoryEntry, TurnDriver,
};

use crate::agent::AcpAgent;
use crate::protocol::AcpError;
use crate::session::AcpHarness;

impl<D> AcpAgent<D>
where
    D: TurnDriver,
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
                title,
            } = callback
            else {
                return Err(AcpError::InvalidResponse(
                    "interactive callback drain returned a non-open callback".to_owned(),
                ));
            };
            if metadata.session_id != harness.session_id {
                return Err(AcpError::InvalidResponse(format!(
                    "callback `{callback_id}` crossed from session `{}` into `{}`",
                    metadata.session_id, harness.session_id
                )));
            }
            // ACP forwards the detail verbatim as the permission request's raw
            // input, so the typed union is rendered back to its wire form here
            // rather than being re-modeled a second time in this adapter.
            let wire = serde_json::to_value(&detail).unwrap_or(Value::Null);
            let output = match detail {
                CallbackDetail::Approval { .. } => {
                    self.resolve_approval_callback(harness, &callback_id, &title, &wire)
                        .await
                }
                // ACP v1 has no interactive question flow, so the canonical
                // request is declined instead of being surfaced.
                CallbackDetail::UserInput { .. } => json!({
                    "type": "user_input",
                    "result": {
                        "answers": [],
                        "cancelled": true,
                    },
                }),
            };
            harness.service.lock().await.respond_callback(json!({
                "sessionId": harness.session_id,
                "callbackId": callback_id,
                "output": output,
            }))?;
        }
        Ok(())
    }

    async fn resolve_approval_callback(
        &self,
        harness: &Arc<AcpHarness<D>>,
        callback_id: &str,
        title: &str,
        detail: &Value,
    ) -> Value {
        let options = acp_approval_options(detail);
        if options.is_empty() {
            return cancelled_approval_output();
        }
        let permission = self.request_permission(
            &harness.session_id,
            json!({
                "toolCallId": callback_id,
                "title": title,
                "kind": "execute",
                "rawInput": detail,
                "_meta": {
                    "callbackId": callback_id,
                    "toolName": detail.pointer("/effect/toolName"),
                },
            }),
            options.clone(),
        );
        let response = tokio::select! {
            response = permission => response.ok(),
            () = harness.cancelled() => None,
        };
        approval_output_from_acp(response.as_ref(), &options)
    }
}

/// Maps the canonical approval choices onto ACP permission options, dropping
/// any choice the canonical layer did not offer.
pub(crate) fn acp_approval_options(detail: &Value) -> Vec<Value> {
    const CHOICES: [(&str, &str, &str, &str); 4] = [
        ("approve", "allow_once", "Allow once", "allow_once"),
        (
            "approve_for_session",
            "allow_always",
            "Allow for this session",
            "allow_always",
        ),
        (
            "approve_permanently",
            "allow_always_permanent",
            "Always allow",
            "allow_always",
        ),
        ("deny", "reject_once", "Reject", "reject_once"),
    ];
    let offered = detail.get("choices").and_then(Value::as_array);
    CHOICES
        .into_iter()
        .filter(|(choice, ..)| {
            offered.is_none_or(|offered| {
                offered
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|value| value == *choice)
            })
        })
        .map(
            |(_, option_id, name, kind)| json!({"optionId": option_id, "name": name, "kind": kind}),
        )
        .collect()
}

/// Anything other than an explicit selection of an offered option cancels the
/// turn, so a malformed client response can never approve work.
pub(crate) fn approval_output_from_acp(response: Option<&Value>, options: &[Value]) -> Value {
    let selected = response
        .filter(|response| {
            response.pointer("/outcome/outcome").and_then(Value::as_str) == Some("selected")
        })
        .and_then(|response| response.pointer("/outcome/optionId"))
        .and_then(Value::as_str)
        .filter(|option_id| {
            options
                .iter()
                .any(|option| option.get("optionId").and_then(Value::as_str) == Some(*option_id))
        });
    let decision = match selected {
        Some("allow_once") => "approve",
        Some("allow_always") => "approve_for_session",
        Some("allow_always_permanent") => "approve_permanently",
        Some("reject_once" | "reject_always") => "deny",
        _ => "cancel_turn",
    };
    json!({
        "type": "approval",
        "decision": {"type": decision},
    })
}

fn cancelled_approval_output() -> Value {
    json!({
        "type": "approval",
        "decision": {"type": "cancel_turn"},
    })
}
