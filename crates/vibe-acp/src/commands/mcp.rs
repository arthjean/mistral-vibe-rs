//! `/mcp`: inspect MCP status or manage a server session.

use serde_json::{Value, json};
use vibe_app_server::client::TurnDriver;

use crate::commands::CommandSink;

const USAGE: &str = "Usage: `/mcp status`, `/mcp login <alias>`, or `/mcp logout <alias>`";

pub(crate) async fn execute<D>(sink: &CommandSink<'_, D>, arguments: &str) -> String
where
    D: TurnDriver,
{
    let (method, alias) = match arguments.split_whitespace().collect::<Vec<_>>().as_slice() {
        [] | ["status"] => ("mcp/read", None),
        ["login", alias] => ("mcp/login", Some((*alias).to_owned())),
        ["logout", alias] => ("mcp/logout", Some((*alias).to_owned())),
        ["status", ..] => return "Usage: `/mcp status`".to_owned(),
        ["login"] => return "Usage: `/mcp login <alias>`".to_owned(),
        ["logout"] => return "Usage: `/mcp logout <alias>`".to_owned(),
        _ => return USAGE.to_owned(),
    };
    let mut params = json!({"sessionId": sink.session_id()});
    if let Some(alias) = alias {
        params["alias"] = json!(alias);
    }
    let mut service = sink.harness.service.lock().await;
    match service.public_call_async(method, params).await {
        Ok(dispatch) if method == "mcp/read" => {
            let state = dispatch.result.get("mcp").cloned().unwrap_or(Value::Null);
            format!(
                "### MCP status\n\n{}",
                serde_json::to_string_pretty(&state).unwrap_or_else(|_| "unavailable".to_owned())
            )
        }
        Ok(_) => format!("MCP {} completed.", method.trim_start_matches("mcp/")),
        Err(error) => format!("MCP command failed: {error}"),
    }
}
