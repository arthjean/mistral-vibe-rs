use serde_json::{Map, Value, json};

use super::super::pickers::mcp_overlay;
use super::super::state::{EntryStatus, TuiState};
use super::super::{
    InteractiveRuntime, call_runtime, push_json_notice, push_local_notice,
    refresh_server_banner_metrics,
};
use super::map_value;

pub(super) fn show_mcp(
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    filter: Option<&str>,
) {
    let Some(result) = call_runtime(runtime, "mcp/read", json!({}), state) else {
        return;
    };
    let mut overlay = mcp_overlay(&map_value(result));
    if let Some(filter) = filter {
        overlay.set_query(filter);
    }
    if overlay.items.is_empty() {
        push_local_notice(
            state,
            "No MCP servers or connectors configured.",
            EntryStatus::Completed,
        );
    } else {
        state.overlay = Some(overlay);
    }
}

pub(super) fn handle_mcp(arguments: &str, runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(parts) = shlex::split(arguments) else {
        state.push_diagnostic("Invalid quoting in /mcp arguments");
        return;
    };
    match parts.as_slice() {
        [] => show_mcp(runtime, state, None),
        [subcommand] if subcommand == "status" => show_mcp_status(runtime, state),
        [subcommand, name] if subcommand == "login" || subcommand == "logout" => {
            let method = if subcommand == "login" {
                "mcp/login"
            } else {
                "mcp/logout"
            };
            if let Some(result) = call_runtime(
                runtime,
                method,
                json!({"sessionId": runtime.session_id, "name": name}),
                state,
            ) {
                let value = map_value(result);
                push_json_notice(state, "MCP authentication", Some(&value));
            }
        }
        [subcommand] if subcommand == "login" || subcommand == "logout" => {
            state.push_diagnostic(format!("Usage: /mcp {subcommand} <alias>"));
        }
        [subcommand, rest @ ..] if subcommand == "add" => add_mcp(rest, runtime, state),
        [name] => show_mcp(runtime, state, Some(name)),
        _ => state.push_diagnostic(
            "Usage: /mcp [name|status|login <alias>|logout <alias>|add <url> [--name <alias>] [--transport <kind>]]",
        ),
    }
}

fn add_mcp(arguments: &[String], runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    const USAGE: &str = "Usage: /mcp add <url> [--name <alias>] [--scope <scope> ...] [--transport http|streamable-http] [--no-login]";
    let mut url = None;
    let mut name_seen = false;
    let mut transport_seen = false;
    let mut params = Map::from_iter([("sessionId".to_owned(), json!(runtime.session_id))]);
    let mut scopes = Vec::new();
    let mut login = true;
    let mut index = 0;
    while index < arguments.len() {
        let key = arguments[index].as_str();
        if key == "--no-login" {
            login = false;
            index += 1;
            continue;
        }
        match key {
            "--name" => {
                if name_seen {
                    state.push_diagnostic("Usage: /mcp add accepts --name only once");
                    return;
                }
                let Some(value) = mcp_option_value(arguments, index, "--name", state) else {
                    return;
                };
                name_seen = true;
                params.insert("name".to_owned(), json!(value));
                index += 2;
            }
            "--transport" => {
                if transport_seen {
                    state.push_diagnostic("Usage: /mcp add accepts --transport only once");
                    return;
                }
                let Some(value) = mcp_option_value(arguments, index, "--transport", state) else {
                    return;
                };
                if !matches!(value.as_str(), "http" | "streamable-http") {
                    state.push_diagnostic("/mcp add transport must be `http` or `streamable-http`");
                    return;
                }
                transport_seen = true;
                params.insert("transport".to_owned(), json!(value));
                index += 2;
            }
            "--scope" => {
                let Some(value) = mcp_option_value(arguments, index, "--scope", state) else {
                    return;
                };
                scopes.push(value.clone());
                index += 2;
            }
            option if option.starts_with("--") => {
                state.push_diagnostic(format!("Unknown /mcp add option `{key}`"));
                return;
            }
            value if url.is_none() => {
                url = Some(value.to_owned());
                index += 1;
            }
            _ => {
                state.push_diagnostic(USAGE);
                return;
            }
        }
    }
    let Some(url) = url else {
        state.push_diagnostic(USAGE);
        return;
    };
    if !transport_seen {
        params.insert("transport".to_owned(), json!("streamable-http"));
    }
    params.insert("url".to_owned(), json!(url));
    params.insert("scopes".to_owned(), json!(scopes));
    params.insert("login".to_owned(), json!(login));
    if let Some(result) = call_runtime(runtime, "mcp/add", Value::Object(params), state) {
        let value = map_value(result);
        push_json_notice(state, "MCP server added", Some(&value));
        refresh_server_banner_metrics(&mut runtime.service, &mut runtime.banner);
    }
}

fn mcp_option_value<'a>(
    arguments: &'a [String],
    index: usize,
    option: &str,
    state: &mut TuiState,
) -> Option<&'a String> {
    let value = arguments
        .get(index.saturating_add(1))
        .filter(|value| !value.starts_with("--"));
    if value.is_none() {
        state.push_diagnostic(format!("Missing value after `{option}`"));
    }
    value
}

fn show_mcp_status(runtime: &mut InteractiveRuntime, state: &mut TuiState) {
    let Some(result) = call_runtime(runtime, "mcp/read", json!({}), state) else {
        return;
    };
    let value = map_value(result);
    let sources = value
        .pointer("/mcp/sources")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if sources.is_empty() {
        push_local_notice(state, "No MCP servers configured.", EntryStatus::Completed);
        return;
    }
    let mut lines = vec!["### MCP auth status".to_owned(), String::new()];
    for source in sources {
        let name = source
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        let status = source
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("unknown");
        lines.push(format!("- `{name}`: `{status}`"));
    }
    push_local_notice(state, &lines.join("\n"), EntryStatus::Completed);
}

pub(super) fn refresh_selected_mcp(runtime: &mut Option<InteractiveRuntime>, state: &mut TuiState) {
    let Some(name) = state
        .overlay
        .as_ref()
        .and_then(|overlay| overlay.selected_item())
        .map(|item| item.id.clone())
    else {
        return;
    };
    let Some(runtime) = runtime.as_mut() else {
        return;
    };
    if call_runtime(
        runtime,
        "mcp/refresh",
        json!({"sessionId": runtime.session_id, "name": name}),
        state,
    )
    .is_some()
    {
        show_mcp(runtime, state, None);
    }
}

pub(super) fn source_enabled(
    runtime: &mut InteractiveRuntime,
    name: &str,
    state: &mut TuiState,
) -> Option<bool> {
    call_runtime(runtime, "mcp/read", json!({}), state)?
        .get("mcp")?
        .get("sources")?
        .as_array()?
        .iter()
        .find(|source| source.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|source| source.get("enabled").and_then(Value::as_bool))
}
