//! Reading the canonical session payloads the app server publishes, and the
//! cursor the ACP listing pages with.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::protocol::{AcpError, AcpSessionInfo};

const SESSION_CURSOR_PREFIX: &str = "v1:";

pub(crate) fn metadata_session_id(result: &BTreeMap<String, Value>) -> Result<String, AcpError> {
    result
        .get("metadata")
        .and_then(|metadata| {
            metadata
                .get("session_id")
                .or_else(|| metadata.get("sessionId"))
        })
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| AcpError::InvalidResponse("missing resumed session ID".to_owned()))
}

pub(crate) fn acp_session_info(session: &Value) -> Result<AcpSessionInfo, AcpError> {
    let session_id = session
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| AcpError::InvalidResponse("listed session omitted id".to_owned()))?
        .to_owned();
    let cwd = session
        .get("workingDirectory")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            AcpError::InvalidResponse(format!("listed session `{session_id}` omitted cwd"))
        })?
        .to_owned();
    let mut meta = BTreeMap::new();
    for key in ["parentSessionId", "messageCount"] {
        if let Some(value) = session.get(key)
            && !value.is_null()
        {
            meta.insert(key.to_owned(), value.clone());
        }
    }
    Ok(AcpSessionInfo {
        session_id,
        cwd,
        title: session
            .get("title")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        updated_at: session
            .get("endTime")
            .and_then(Value::as_str)
            .or_else(|| session.get("startTime").and_then(Value::as_str))
            .map(ToOwned::to_owned),
        additional_directories: None,
        meta,
    })
}

pub(crate) fn decode_session_cursor(cursor: Option<&str>) -> Result<usize, AcpError> {
    let Some(cursor) = cursor else {
        return Ok(0);
    };
    cursor
        .strip_prefix(SESSION_CURSOR_PREFIX)
        .and_then(|offset| offset.parse::<usize>().ok())
        .ok_or_else(|| AcpError::InvalidParams("invalid session/list cursor".to_owned()))
}

pub(crate) fn encode_session_cursor(offset: usize) -> String {
    format!("{SESSION_CURSOR_PREFIX}{offset}")
}
