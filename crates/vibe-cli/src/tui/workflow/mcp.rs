use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};
use url::Url;

use super::super::clipboard::copy_text_bounded;
use super::super::command_handlers::mcp_authenticated;
use super::super::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, IntegrationTarget, Overlay, OverlayAction,
    OverlayItem, OverlayKind,
};
use super::super::pickers::{
    mcp_auth_failed_overlay, mcp_auth_overlay, mcp_detail_overlay, mcp_overlay,
};
use super::super::runtime::{schedule_ui_background, schedule_ui_call, schedule_ui_external};
use super::super::state::{EntryStatus, TuiState};
use super::super::{InteractiveRuntime, UiOperation, push_local_document, push_local_notice};
use super::map_value;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum McpEffect {
    Show {
        filter: Option<String>,
    },
    ShowDetail {
        target: IntegrationTarget,
    },
    BeginAuth {
        kind: IntegrationKind,
        source: String,
        enable_on_complete: bool,
    },
    SetEnabled {
        target: IntegrationTarget,
        detail: bool,
        enabled: bool,
    },
    OpenUrl {
        url: String,
    },
    CopyUrl {
        url: String,
    },
    /// Reference `_toggle_url`: the panel prints the URL, or stops printing it.
    ToggleUrl {
        action: AuthAction,
    },
    /// Reference `MCPApp.action_refresh`: re-read every source and keep the
    /// view that was open.
    RefreshAll {
        detail: Option<IntegrationTarget>,
    },
    /// Reference `ConnectorAuthApp.action_refresh`: ask whether the connector
    /// signed in.
    CheckConnector {
        source: String,
    },
}

#[derive(Debug, Clone)]
pub(in crate::tui) enum McpPendingOperation {
    ReadIntegrations {
        filter: Option<String>,
        detail: Option<IntegrationTarget>,
    },
    BeginAuth {
        kind: IntegrationKind,
        source: String,
        enable_on_complete: bool,
    },
    RefreshAll {
        detail: Option<IntegrationTarget>,
    },
    CheckConnector {
        source: String,
    },
    EnableAfterAuthentication,
    SetEnabled {
        target: IntegrationTarget,
        detail: bool,
    },
    CopyUrl,
    OpenUrl,
    /// A server login published the URL that authorizes it.
    LoginUrl {
        source: String,
        origin: McpLoginOrigin,
    },
    /// A server login answered: the browser came back, or the login failed.
    LoginFinished {
        source: String,
        origin: McpLoginOrigin,
    },
}

/// Who started a server login, which decides where its URL is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::tui) enum McpLoginOrigin {
    /// `/mcp login` or `/mcp add`: the URL goes to the transcript and the
    /// browser, reference `_mcp_login`.
    Command,
    /// The integrations overlay: the URL goes to the authentication overlay,
    /// reference `MCPOAuthApp`.
    Overlay { enable_on_complete: bool },
}

/// Runs a server login beside the interactive slot, reference
/// `MCPResource.login`: the call blocks until the browser comes back, and the
/// URL it publishes on the way reaches the operator as it is published.
pub(in crate::tui) fn schedule_mcp_login(
    runtime: &mut InteractiveRuntime,
    source: String,
    origin: McpLoginOrigin,
    state: &mut TuiState,
) -> bool {
    let name = source.clone();
    let progress_origin = origin.clone();
    schedule_ui_background(
        runtime,
        "mcp_catalog/login",
        json!({"name": source}),
        move |notification| {
            // The catalog publishes `mcp/authUrl` beside the canonical name;
            // reference `consume_notification` reads only the canonical one.
            (notification.method == "mcp_catalog/authUrl"
                && notification.params.get("name").and_then(Value::as_str) == Some(&name))
            .then(|| {
                UiOperation::Mcp(McpPendingOperation::LoginUrl {
                    source: name.clone(),
                    origin: progress_origin.clone(),
                })
            })
        },
        UiOperation::Mcp(McpPendingOperation::LoginFinished { source, origin }),
        state,
    )
}

type UrlOpenFuture = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'static>>;

pub(in crate::tui) trait UrlOpenerPort {
    fn open(&self, url: String) -> UrlOpenFuture;
}

pub(in crate::tui) struct SystemUrlOpener;

impl UrlOpenerPort for SystemUrlOpener {
    fn open(&self, url: String) -> UrlOpenFuture {
        Box::pin(open_auth_url(url))
    }
}

#[must_use]
/// What one row or key of the authentication panel does, or `None` when the
/// reference does nothing: `r` while a server login is still waiting on the
/// browser (`MCPOAuthApp.action_refresh` ignores it then).
pub(in crate::tui) fn reduce_auth_action(action: &AuthAction) -> Option<McpEffect> {
    match action.action {
        AuthActionKind::Open => Some(McpEffect::OpenUrl {
            url: action.url.clone(),
        }),
        AuthActionKind::Copy => Some(McpEffect::CopyUrl {
            url: action.url.clone(),
        }),
        AuthActionKind::Show => Some(McpEffect::ToggleUrl {
            action: action.clone(),
        }),
        AuthActionKind::Refresh => match action.kind {
            IntegrationKind::Connector => Some(McpEffect::CheckConnector {
                source: action.source.clone(),
            }),
            // A failed login left no URL, and `r` retries it.
            IntegrationKind::McpServer if action.url.is_empty() => Some(McpEffect::BeginAuth {
                kind: action.kind,
                source: action.source.clone(),
                enable_on_complete: action.enable_on_complete,
            }),
            IntegrationKind::McpServer => None,
        },
        // Reference `MCPOAuthClosed(refreshed=False)` and its connector twin:
        // the browser opens again.
        AuthActionKind::Close => Some(McpEffect::Show { filter: None }),
    }
}

/// The panel's own context, which every row of it carries.
pub(in crate::tui) fn auth_panel_context(state: &TuiState) -> Option<AuthAction> {
    let overlay = state.overlay.as_ref()?;
    if overlay.kind != OverlayKind::McpAuth {
        return None;
    }
    overlay.items.iter().find_map(|item| match &item.action {
        OverlayAction::Authenticate(action) => Some(action.clone()),
        _ => None,
    })
}

pub(in crate::tui) fn execute_mcp_effect(
    effect: McpEffect,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
    opener: &dyn UrlOpenerPort,
) {
    match effect {
        McpEffect::Show { filter } => {
            schedule_read_integrations(runtime, filter, None, state);
        }
        McpEffect::ShowDetail { target } => {
            schedule_read_integrations(runtime, None, Some(target), state);
        }
        McpEffect::BeginAuth {
            kind: IntegrationKind::McpServer,
            source,
            enable_on_complete,
        } => {
            schedule_mcp_login(
                runtime,
                source,
                McpLoginOrigin::Overlay { enable_on_complete },
                state,
            );
        }
        McpEffect::BeginAuth {
            kind,
            source,
            enable_on_complete,
        } => {
            schedule_ui_call(
                runtime,
                "connectors/auth/read",
                json!({"name": source}),
                UiOperation::Mcp(McpPendingOperation::BeginAuth {
                    kind,
                    source,
                    enable_on_complete,
                }),
                state,
            );
        }
        McpEffect::SetEnabled {
            target,
            detail,
            enabled,
        } => {
            let (method, params) = match target.kind {
                IntegrationKind::Connector => (
                    "connectors/toggle",
                    json!({
                        "name": target.source,
                        "toolName": target.tool,
                        "disabled": !enabled,
                    }),
                ),
                IntegrationKind::McpServer => (
                    "mcp/toggle",
                    json!({
                        "name": target.source,
                        "source": "server",
                        "toolName": target.tool,
                        "disabled": !enabled,
                    }),
                ),
            };
            schedule_ui_call(
                runtime,
                method,
                params,
                UiOperation::Mcp(McpPendingOperation::SetEnabled { target, detail }),
                state,
            );
        }
        McpEffect::OpenUrl { url } => {
            schedule_ui_external(
                runtime,
                UiOperation::Mcp(McpPendingOperation::OpenUrl),
                opener.open(url),
                state,
            );
        }
        McpEffect::CopyUrl { url } => {
            schedule_ui_external(
                runtime,
                UiOperation::Mcp(McpPendingOperation::CopyUrl),
                async move {
                    copy_text_bounded(url)
                        .await
                        .map_err(|_| "Authentication URL could not be copied".to_owned())
                },
                state,
            );
        }
        McpEffect::ToggleUrl { action } => {
            let mut overlay = mcp_auth_overlay(
                action.kind,
                &action.source,
                &action.url,
                action.enable_on_complete,
                !action.url_visible,
            );
            overlay.select_id("auth:show");
            state.overlay = Some(overlay);
        }
        McpEffect::RefreshAll { detail } => {
            // The MCP catalog refreshes the whole session, reference
            // `MCPRefreshParams`, which names no server.
            schedule_ui_call(
                runtime,
                "mcp/refresh",
                json!({}),
                UiOperation::Mcp(McpPendingOperation::RefreshAll { detail }),
                state,
            );
        }
        McpEffect::CheckConnector { source } => {
            schedule_ui_call(
                runtime,
                "connectors/refresh",
                json!({"name": source}),
                UiOperation::Mcp(McpPendingOperation::CheckConnector { source }),
                state,
            );
        }
    }
}

/// Reads every integration in one call.
///
/// `MCPState` carries the servers and the connectors in one list, so the two
/// reads this used to chain are one, and the split back into the two families a
/// picker renders happens here rather than on the wire.
fn schedule_read_integrations(
    runtime: &mut InteractiveRuntime,
    filter: Option<String>,
    detail: Option<IntegrationTarget>,
    state: &mut TuiState,
) {
    schedule_ui_call(
        runtime,
        "mcp/read",
        json!({}),
        UiOperation::Mcp(McpPendingOperation::ReadIntegrations { filter, detail }),
        state,
    );
}

/// The MCP servers of a published source list, under the key the pickers read.
pub(super) fn server_sources(value: &Value) -> Value {
    json!({"mcp": {"sources": sources_of(value, "server").collect::<Vec<_>>()}})
}

/// The connectors of a published source list, in the shape the connector picker
/// reads: it renders an authorization state and a flat tool list, and the
/// published source carries both under other names.
pub(super) fn connector_sources(value: &Value) -> Value {
    json!({
        "connectors": {
            "sources": sources_of(value, "connector")
                .map(|source| {
                    let tools = source.get("tools").and_then(Value::as_array);
                    let name = source.get("name").and_then(Value::as_str).unwrap_or_default();
                    let status = source.get("status").and_then(Value::as_str).unwrap_or_default();
                    json!({
                        "id": name,
                        "alias": name,
                        "name": name,
                        "enabled": status != "disabled",
                        "authState": match status {
                            "connected" => "connected",
                            "needs_auth" => "disconnected",
                            "needs_setup" => "setup_required",
                            _ => "failed",
                        },
                        "toolNames": tools
                            .into_iter()
                            .flatten()
                            .filter_map(|tool| tool.get("name").cloned())
                            .collect::<Vec<_>>(),
                        "disabledTools": tools
                            .into_iter()
                            .flatten()
                            .filter(|tool| tool.get("enabled").and_then(Value::as_bool) == Some(false))
                            .filter_map(|tool| tool.get("name").cloned())
                            .collect::<Vec<_>>(),
                    })
                })
                .collect::<Vec<_>>()
        }
    })
}

fn sources_of<'a>(value: &'a Value, kind: &'a str) -> impl Iterator<Item = &'a Value> {
    value
        .pointer("/mcp/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(move |source| source.get("kind").and_then(Value::as_str) == Some(kind))
}

pub(in crate::tui) fn apply_pending_operation(
    operation: McpPendingOperation,
    result: Result<vibe_app_server::client::PublicDispatch, String>,
    runtime: &mut InteractiveRuntime,
    state: &mut TuiState,
) {
    let dispatch = match result {
        Ok(dispatch) => dispatch,
        Err(error) => {
            // Reference `MCPOAuthApp._on_login_failed`: the panel stays, says
            // so, and `r` retries.
            if let McpPendingOperation::LoginFinished {
                source,
                origin: McpLoginOrigin::Overlay { enable_on_complete },
            } = &operation
                && auth_panel_context(state).is_some_and(|panel| &panel.source == source)
            {
                state.overlay = Some(mcp_auth_failed_overlay(source, *enable_on_complete));
            }
            state.push_diagnostic(error);
            return;
        }
    };
    match operation {
        McpPendingOperation::LoginUrl { source, origin } => {
            let url = dispatch
                .notifications
                .first()
                .and_then(|notification| notification.params.get("url"))
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
                .map(ToOwned::to_owned);
            let Some(url) = url else {
                state.push_diagnostic("Authentication source returned no URL");
                return;
            };
            match origin {
                McpLoginOrigin::Command => {
                    push_local_document(
                        state,
                        format!("Open this URL in your browser:\n\n  {url}"),
                    );
                    // Reference `webbrowser.open` is fire and forget: a
                    // browser that will not open leaves the URL printed above.
                    tokio::spawn(async move {
                        drop(open_auth_url(url).await);
                    });
                }
                McpLoginOrigin::Overlay { enable_on_complete } => {
                    state.overlay = Some(mcp_auth_overlay(
                        IntegrationKind::McpServer,
                        &source,
                        &url,
                        enable_on_complete,
                        false,
                    ));
                }
            }
            return;
        }
        McpPendingOperation::LoginFinished { source, origin } => {
            match origin {
                McpLoginOrigin::Command => {
                    push_local_document(state, mcp_authenticated(&source));
                }
                // Reference `MCPOAuthClosed(refreshed=True)`: the browser
                // refreshes and opens on the server that signed in.
                McpLoginOrigin::Overlay { enable_on_complete } => {
                    let target = IntegrationTarget {
                        kind: IntegrationKind::McpServer,
                        source: source.clone(),
                        tool: None,
                        enabled: true,
                        requires_auth: false,
                        requires_setup: false,
                    };
                    if enable_on_complete {
                        schedule_ui_call(
                            runtime,
                            "mcp/toggle",
                            json!({"name": source, "source": "server", "toolName": null, "disabled": false}),
                            UiOperation::Mcp(McpPendingOperation::EnableAfterAuthentication),
                            state,
                        );
                    } else {
                        execute_refresh_all(runtime, Some(target), state);
                    }
                }
            }
            return;
        }
        _ => {}
    }
    let value = map_value(dispatch.result);
    match operation {
        McpPendingOperation::ReadIntegrations { filter, detail } => {
            let servers = server_sources(&value);
            let connectors = connector_sources(&value);
            if let Some(target) = detail {
                state.overlay = Some(mcp_detail_overlay(&servers, &connectors, &target));
            } else {
                let mut overlay = mcp_overlay(&servers, &connectors);
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
        }
        McpPendingOperation::BeginAuth {
            kind,
            source,
            enable_on_complete,
        } => {
            // Only a connector sign-in comes here, and it answers with the URL.
            match value
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty())
            {
                Some(url) => {
                    state.overlay = Some(mcp_auth_overlay(
                        kind,
                        &source,
                        url,
                        enable_on_complete,
                        false,
                    ));
                }
                // Reference `_on_auth_url_fetched` without a URL.
                None => {
                    state.overlay = Some(Overlay::new(
                        OverlayKind::McpAuth,
                        format!("Connector: {source}"),
                        vec![OverlayItem::new(
                            "auth:unavailable",
                            "This connector does not provide authentication",
                            "",
                            true,
                        )],
                    ));
                }
            }
        }
        McpPendingOperation::RefreshAll { detail } => {
            schedule_read_integrations(runtime, None, detail, state);
        }
        McpPendingOperation::CheckConnector { source } => {
            // The refresh answers with the runtime it produced, so the
            // connector's new state is read off the published source list.
            let connected = sources_of(value.get("runtime").unwrap_or(&Value::Null), "connector")
                .find(|candidate| candidate.get("name").and_then(Value::as_str) == Some(&source))
                .and_then(|candidate| candidate.get("status"))
                .and_then(Value::as_str)
                == Some("connected");
            if connected {
                // Reference `ConnectorAuthClosed(refreshed=True)`: the browser
                // opens on the connector that signed in.
                let target = IntegrationTarget {
                    kind: IntegrationKind::Connector,
                    source,
                    tool: None,
                    enabled: true,
                    requires_auth: false,
                    requires_setup: false,
                };
                schedule_read_integrations(runtime, None, Some(target), state);
            } else {
                state.push_diagnostic(
                    "The connector found no tools yet; finish signing in, then press r again",
                );
            }
        }
        McpPendingOperation::EnableAfterAuthentication => {
            schedule_read_integrations(runtime, None, None, state);
        }
        McpPendingOperation::SetEnabled { target, detail } => {
            schedule_read_integrations(runtime, None, detail.then_some(target), state);
        }
        // Answered before the dispatch is read as a runtime.
        McpPendingOperation::LoginUrl { .. } | McpPendingOperation::LoginFinished { .. } => {}
        // Reference `copy_text_to_clipboard(success_message=...)` and the
        // panel's "Opened in browser." status.
        McpPendingOperation::CopyUrl => state.push_diagnostic("Auth URL copied to clipboard"),
        McpPendingOperation::OpenUrl => state.push_diagnostic("Opened in browser."),
    }
}

fn execute_refresh_all(
    runtime: &mut InteractiveRuntime,
    detail: Option<IntegrationTarget>,
    state: &mut TuiState,
) {
    schedule_ui_call(
        runtime,
        "mcp/refresh",
        json!({}),
        UiOperation::Mcp(McpPendingOperation::RefreshAll { detail }),
        state,
    );
}

/// Whether a published sign-in URL may be handed to the system opener.
///
/// The reference passes whatever the server returned to `webbrowser.open`;
/// this port shows it the same way but opens only a web page, so a server
/// cannot make the terminal launch a `file:` or custom-scheme handler.
pub(in crate::tui) fn openable_auth_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| matches!(url.scheme(), "https" | "http"))
}

pub(super) async fn open_auth_url(url: String) -> Result<(), String> {
    if !openable_auth_url(&url) {
        return Err("Only an http or https authentication URL can be opened".to_owned());
    }
    let candidates: &[(&str, &[&str])] = if cfg!(target_os = "macos") {
        &[("open", &[])]
    } else if cfg!(target_os = "windows") {
        &[("cmd", &["/C", "start", ""])]
    } else {
        &[("xdg-open", &[]), ("gio", &["open"])]
    };
    for (program, arguments) in candidates {
        let mut arguments = arguments.to_vec();
        arguments.push(&url);
        if super::super::external_action::run_command(program, &arguments, None)
            .await
            .is_ok()
        {
            return Ok(());
        }
    }
    Err("Authentication URL could not be opened".to_owned())
}

/// Reference `MCPApp` binding `r`: refresh every source and stay on the view
/// that was open, the list or one source's detail.
pub(super) fn refresh_mcp_view(state: &TuiState) -> Option<McpEffect> {
    let overlay = state.overlay.as_ref()?;
    let detail = (overlay.kind == OverlayKind::McpDetail)
        .then(|| {
            overlay.items.iter().find_map(|item| match &item.action {
                OverlayAction::Integration(target) => Some(IntegrationTarget {
                    tool: None,
                    ..target.clone()
                }),
                _ => None,
            })
        })
        .flatten();
    Some(McpEffect::RefreshAll { detail })
}

pub(super) fn set_selected_mcp(state: &mut TuiState, enabled: bool) -> Option<McpEffect> {
    let (target, detail) = selected_integration(state)?;
    if detail && target.tool.is_none() {
        return None;
    }
    Some(McpEffect::SetEnabled {
        target,
        detail,
        enabled,
    })
}

fn selected_integration(state: &TuiState) -> Option<(IntegrationTarget, bool)> {
    let overlay = state.overlay.as_ref()?;
    let target = match &overlay.selected_item()?.action {
        OverlayAction::Integration(target) => target.clone(),
        _ => return None,
    };
    Some((target, overlay.kind == OverlayKind::McpDetail))
}

#[cfg(test)]
mod tests {
    use super::{McpEffect, openable_auth_url, reduce_auth_action};
    use crate::tui::interaction::{AuthAction, AuthActionKind, IntegrationKind};

    #[test]
    fn only_a_web_page_is_handed_to_the_system_opener() {
        assert!(openable_auth_url("https://auth.example/authorize"));
        assert!(openable_auth_url("http://auth.example/authorize"));
        assert!(openable_auth_url("http://127.0.0.1/callback"));
        assert!(!openable_auth_url("file:///tmp/token"));
        assert!(!openable_auth_url("vscode://callback"));
        assert!(!openable_auth_url("not a URL"));
    }

    #[test]
    fn authentication_rows_and_keys_reduce_as_the_reference_panels_act() {
        let action = AuthAction {
            kind: IntegrationKind::Connector,
            source: "drive".to_owned(),
            url: "https://auth.example/drive".to_owned(),
            action: AuthActionKind::Copy,
            enable_on_complete: false,
            url_visible: false,
        };
        assert_eq!(
            reduce_auth_action(&action),
            Some(McpEffect::CopyUrl {
                url: "https://auth.example/drive".to_owned(),
            })
        );
        let refresh = AuthAction {
            action: AuthActionKind::Refresh,
            ..action.clone()
        };
        assert_eq!(
            reduce_auth_action(&refresh),
            Some(McpEffect::CheckConnector {
                source: "drive".to_owned(),
            })
        );
        // A server login still waiting on the browser ignores `r`; a failed
        // one, which left no URL, starts again.
        let waiting = AuthAction {
            kind: IntegrationKind::McpServer,
            ..refresh
        };
        assert_eq!(reduce_auth_action(&waiting), None);
        let failed = AuthAction {
            url: String::new(),
            ..waiting
        };
        assert_eq!(
            reduce_auth_action(&failed),
            Some(McpEffect::BeginAuth {
                kind: IntegrationKind::McpServer,
                source: "drive".to_owned(),
                enable_on_complete: false,
            })
        );
        let close = AuthAction {
            action: AuthActionKind::Close,
            ..failed
        };
        assert_eq!(
            reduce_auth_action(&close),
            Some(McpEffect::Show { filter: None })
        );
    }
}
