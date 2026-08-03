//! Canonical history paging shared by prompts, forks, and replay.

use serde_json::{Value, json};
use vibe_app_server::client::{HeadlessService, TurnDriver};

use crate::protocol::AcpError;

pub(crate) const HISTORY_PAGE_SIZE: usize = 500;
pub(crate) const MAX_HISTORY_PAGE_SIZE: usize = 500;
const MAX_HISTORY_ENTRIES: usize = 100_000;

pub(crate) fn history_page<D>(
    service: &mut HeadlessService<D>,
    session_id: &str,
    offset: usize,
    limit: usize,
) -> Result<Vec<Value>, AcpError>
where
    D: TurnDriver,
{
    Ok(service
        .public_call(
            "history/list",
            json!({"sessionId": session_id, "offset": offset, "limit": limit}),
        )?
        .get("history")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

pub(crate) fn all_history_entries<D>(
    service: &mut HeadlessService<D>,
    session_id: &str,
) -> Result<Vec<Value>, AcpError>
where
    D: TurnDriver,
{
    let mut history = Vec::new();
    loop {
        let page = history_page(service, session_id, history.len(), HISTORY_PAGE_SIZE)?;
        if history.len().saturating_add(page.len()) > MAX_HISTORY_ENTRIES {
            return Err(AcpError::InvalidResponse(format!(
                "history exceeded the {MAX_HISTORY_ENTRIES}-entry limit"
            )));
        }
        let complete = page.len() < HISTORY_PAGE_SIZE;
        history.extend(page);
        if complete {
            return Ok(history);
        }
    }
}

pub(crate) fn parse_history_user_message_id(message_id: &str) -> Option<usize> {
    let mut parts = message_id.split(':');
    match (parts.next(), parts.next(), parts.next(), parts.next()) {
        (Some("history"), Some(index), Some("user"), None) => index.parse().ok(),
        _ => None,
    }
}

/// Number of leading history entries a fork keeps: everything through the turn
/// that starts at `anchor`, up to the next user message.
pub(crate) fn fork_source_keep_messages(
    history: &[Value],
    anchor: usize,
    message_id: &str,
) -> Result<usize, AcpError> {
    let Some(message) = history.get(anchor) else {
        return Err(AcpError::InvalidParams(format!(
            "cannot fork from stale user messageId `{message_id}`"
        )));
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return Err(AcpError::InvalidParams(format!(
            "fork messageId `{message_id}` does not identify a user message"
        )));
    }
    Ok(history
        .iter()
        .enumerate()
        .skip(anchor.saturating_add(1))
        .find(|(_, message)| message.get("role").and_then(Value::as_str) == Some("user"))
        .map_or(history.len(), |(index, _)| index))
}
