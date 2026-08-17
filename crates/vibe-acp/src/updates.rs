//! Translation between canonical app-server entries and ACP session updates.
//!
//! The three directions are kept apart: [`stream`] projects a running turn,
//! [`replay`] projects a saved transcript, and [`prompt`] reads the untrusted
//! input an editor sends. What they share lives here.

pub(crate) mod prompt;
pub(crate) mod replay;
pub(crate) mod stream;

use serde_json::{Value, json};
use vibe_app_server::client::{ProgrammaticTurn, PublicTurnStopReason};

pub(crate) use prompt::turn_request;
pub(crate) use replay::history_entry_updates;
pub(crate) use stream::{AcpUpdateProjection, send_acp_updates};

use crate::protocol::{AcpError, AcpSessionUpdate};

pub const MAX_ACP_UPDATE_QUEUE: usize = 1_024;

pub(crate) type UpdateSender = tokio::sync::mpsc::Sender<AcpSessionUpdate>;

pub(crate) fn queue_update(
    sender: &UpdateSender,
    session_id: &str,
    update: Value,
) -> Result<(), AcpError> {
    sender
        .try_send(AcpSessionUpdate {
            session_id: session_id.to_owned(),
            update,
        })
        .map_err(|error| match error {
            tokio::sync::mpsc::error::TrySendError::Full(_) => AcpError::Backpressure,
            tokio::sync::mpsc::error::TrySendError::Closed(_) => AcpError::Disconnected,
        })
}

pub(crate) fn text_chunk(kind: &str, message_id: &str, text: &str) -> Value {
    content_chunk(kind, message_id, json!({"type": "text", "text": text}))
}

pub(crate) fn content_chunk(kind: &str, message_id: &str, content: Value) -> Value {
    json!({
        "sessionUpdate": kind,
        "content": content,
        "messageId": message_id,
    })
}

pub(crate) fn send_usage(
    session_id: &str,
    turn: &ProgrammaticTurn,
    context_window: u64,
    sender: &UpdateSender,
) -> Result<(), AcpError> {
    let mut update = json!({
        "sessionUpdate": "usage_update",
        "used": turn.context_tokens,
        "size": context_window,
    });
    if turn.usage.price_micros > 0 {
        update["cost"] = json!({
            "amount": turn.usage.price_micros as f64 / 1_000_000.0,
            "currency": "USD",
        });
    }
    queue_update(sender, session_id, update)
}

pub(crate) fn acp_stop_reason(turn: &ProgrammaticTurn) -> &'static str {
    match turn.stop_reason {
        PublicTurnStopReason::Complete => "end_turn",
        PublicTurnStopReason::MaxSteps => "max_turn_requests",
        PublicTurnStopReason::TokenLimit
        | PublicTurnStopReason::PriceLimit
        | PublicTurnStopReason::ResponseLength => "max_tokens",
        PublicTurnStopReason::Refusal => "refusal",
        PublicTurnStopReason::Cancelled | PublicTurnStopReason::Failed => "cancelled",
    }
}
