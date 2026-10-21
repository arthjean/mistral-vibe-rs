//! `/mcp`: the integrations panel and the subcommands that act on one server.

use super::{CommandBackend, Effect, Panel, mcp_arguments};
use crate::tui::interaction::IntegrationKind;

/// Reference `_show_mcp` and `_maybe_handle_mcp_subcommand`.
pub(super) async fn handle<B: CommandBackend>(arguments: &str, backend: &mut B) {
    use mcp_arguments::Subcommand;

    if let Some((subcommand, rest)) = mcp_arguments::parse_subcommand(arguments) {
        match subcommand {
            Subcommand::Add => mcp_add(rest, backend).await,
            Subcommand::Status if !rest.is_empty() => {
                backend.emit(Effect::Error("Usage: /mcp status".to_owned()));
            }
            Subcommand::Status => {
                let statuses = backend.mcp_state().await.statuses;
                if statuses.is_empty() {
                    backend.emit(Effect::Message("No MCP servers configured.".to_owned()));
                } else {
                    let mut lines = vec!["### MCP auth status".to_owned(), String::new()];
                    lines.extend(
                        statuses
                            .iter()
                            .map(|(alias, status)| format!("- `{alias}`: `{status}`")),
                    );
                    backend.emit(Effect::Message(lines.join("\n")));
                }
            }
            Subcommand::Login => mcp_login(rest, backend).await,
            Subcommand::Logout => {
                if rest.is_empty() {
                    backend.emit(Effect::Error("Usage: /mcp logout <alias>".to_owned()));
                } else {
                    match backend.mcp_logout(rest).await {
                        Ok(()) => backend
                            .emit(Effect::Message(format!("MCP server `{rest}` logged out."))),
                        Err(error) => backend.emit(Effect::Error(error)),
                    }
                }
            }
        }
        return;
    }
    let state = backend.mcp_state().await;
    if let Some(error) = &state.connector_error {
        backend.emit(Effect::Error(format!(
            "Could not load connectors.\n{error}"
        )));
    }
    if state.sources.is_empty() {
        if state.connector_error.is_none() {
            backend.emit(Effect::Message(
                "No MCP servers or connectors configured.".to_owned(),
            ));
        }
        return;
    }
    let name = arguments.trim();
    if !name.is_empty() && !state.sources.iter().any(|(source, _)| source == name) {
        let known = state
            .sources
            .iter()
            .map(|(source, _)| source.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        backend.emit(Effect::Error(format!(
            "Unknown MCP server or connector: {name}. Known: {known}"
        )));
        return;
    }
    backend.emit(Effect::Message("MCP and connectors opened...".to_owned()));
    backend.emit(Effect::Panel {
        panel: Panel::Mcp,
        initial: Some(name.to_owned()),
    });
}

/// Reference `_mcp_login` and `_maybe_login_connector`: a name only a
/// connector carries opens the connector's sign-in; a server, including one
/// sharing its alias with a connector, runs the OAuth login.
async fn mcp_login<B: CommandBackend>(alias: &str, backend: &mut B) {
    if alias.is_empty() {
        backend.emit(Effect::Error("Usage: /mcp login <alias>".to_owned()));
        return;
    }
    let state = backend.mcp_state().await;
    let carries = |kind| {
        state
            .sources
            .iter()
            .any(|(name, source_kind)| name == alias && *source_kind == kind)
    };
    if carries(IntegrationKind::Connector) && !carries(IntegrationKind::McpServer) {
        backend.emit(Effect::ClosePanel);
        backend.emit(Effect::Panel {
            panel: Panel::ConnectorAuth,
            initial: Some(alias.to_owned()),
        });
        return;
    }
    let login = match backend.mcp_login(alias).await {
        Ok(login) => login,
        Err(error) => {
            backend.emit(Effect::Error(error));
            return;
        }
    };
    for url in &login.urls {
        backend.emit(Effect::Message(format!(
            "Open this URL in your browser:\n\n  {url}"
        )));
        backend.open_url(url);
    }
    if login.completed {
        backend.emit(Effect::Message(mcp_authenticated(alias)));
    }
}

/// What a completed server login reports.
pub(in crate::tui) fn mcp_authenticated(alias: &str) -> String {
    format!("MCP server `{alias}` authenticated.")
}

/// Reference `_mcp_add`.
async fn mcp_add<B: CommandBackend>(arguments: &str, backend: &mut B) {
    if mcp_arguments::is_add_help_request(arguments) {
        backend.emit(Effect::Message(mcp_arguments::add_help()));
        return;
    }
    let parsed = match mcp_arguments::parse_add(arguments) {
        Ok(parsed) => parsed,
        Err(error) => {
            backend.emit(Effect::Error(error));
            return;
        }
    };
    let (name, created) = match backend.mcp_add(&parsed).await {
        Ok(added) => added,
        Err(error) => {
            backend.emit(Effect::Error(error));
            return;
        }
    };
    let head = if created {
        format!("Added OAuth MCP server `{name}`.")
    } else {
        format!("OAuth MCP server `{name}` is already configured.")
    };
    let tail = if parsed.login {
        "Starting OAuth login...".to_owned()
    } else {
        format!("Run `/mcp login {name}` to authenticate, or `/mcp status` to inspect it.")
    };
    backend.emit(Effect::Message(format!("{head}\n{tail}")));
    if parsed.login {
        mcp_login(&name, backend).await;
    }
}
