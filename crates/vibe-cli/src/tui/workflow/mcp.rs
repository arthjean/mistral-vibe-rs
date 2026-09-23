use std::future::Future;
use std::pin::Pin;

use serde_json::{Value, json};
use url::Url;

use super::super::clipboard::copy_text_bounded;
use super::super::command_handlers::mcp_authenticated;
use super::super::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, IntegrationTarget, OverlayAction, OverlayKind,
};
use super::super::pickers::{mcp_auth_overlay, mcp_detail_overlay, mcp_overlay};
use super::super::runtime::{schedule_ui_call, schedule_ui_external};
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
        kind: IntegrationKind,
        source: String,
        url: String,
        enable_on_complete: bool,
    },
    CopyUrl {
        kind: IntegrationKind,
        source: String,
        url: String,
        enable_on_complete: bool,
    },
    ShowUrl {
        kind: IntegrationKind,
        source: String,
        url: String,
        enable_on_complete: bool,
    },
    Refresh {
        kind: IntegrationKind,
        source: String,
    },
    CompleteAuthentication {
        kind: IntegrationKind,
        source: String,
        enable_source: bool,
    },
    Logout {
        source: String,
    },
    Close,
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
    Refresh {
        kind: IntegrationKind,
        source: String,
    },
    CompleteAuthentication {
        source: String,
        enable_source: bool,
    },
    EnableAfterAuthentication,
    Logout,
    SetEnabled {
        target: IntegrationTarget,
        detail: bool,
    },
    CopyUrl,
    OpenUrl,
    /// A `/mcp login` waiting for the browser: the wait elapsed, and the login
    /// is asked whether it finished.
    AwaitLogin {
        source: String,
        attempt: u32,
    },
    /// What the login answered when asked.
    LoginChecked {
        source: String,
        attempt: u32,
    },
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
pub(in crate::tui) fn reduce_auth_action(action: &AuthAction) -> McpEffect {
    let source = action.source.clone();
    let url = action.url.clone();
    match action.action {
        AuthActionKind::Open => McpEffect::OpenUrl {
            kind: action.kind,
            source,
            url,
            enable_on_complete: action.enable_on_complete,
        },
        AuthActionKind::Copy => McpEffect::CopyUrl {
            kind: action.kind,
            source,
            url,
            enable_on_complete: action.enable_on_complete,
        },
        AuthActionKind::Show => McpEffect::ShowUrl {
            kind: action.kind,
            source,
            url,
            enable_on_complete: action.enable_on_complete,
        },
        AuthActionKind::Refresh => McpEffect::CompleteAuthentication {
            kind: action.kind,
            source,
            enable_source: action.enable_on_complete,
        },
        AuthActionKind::Logout => McpEffect::Logout { source },
        AuthActionKind::Close => McpEffect::Close,
    }
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
            kind,
            source,
            enable_on_complete,
        } => {
            let method = match kind {
                IntegrationKind::Connector => "connectors/auth/read",
                IntegrationKind::McpServer => "mcp/login",
            };
            schedule_ui_call(
                runtime,
                method,
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
            let method = match target.kind {
                IntegrationKind::Connector => "connectors/toggle",
                IntegrationKind::McpServer => "mcp/toggle",
            };
            schedule_ui_call(
                runtime,
                method,
                json!({
                    "name": target.source,
                    "toolName": target.tool,
                    "disabled": !enabled,
                }),
                UiOperation::Mcp(McpPendingOperation::SetEnabled { target, detail }),
                state,
            );
        }
        McpEffect::OpenUrl {
            kind,
            source,
            url,
            enable_on_complete,
        } => {
            state.overlay = Some(mcp_auth_overlay(kind, &source, &url, enable_on_complete));
            schedule_ui_external(
                runtime,
                UiOperation::Mcp(McpPendingOperation::OpenUrl),
                opener.open(url),
                state,
            );
        }
        McpEffect::CopyUrl {
            kind,
            source,
            url,
            enable_on_complete,
        } => {
            state.overlay = Some(mcp_auth_overlay(kind, &source, &url, enable_on_complete));
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
        McpEffect::ShowUrl {
            kind,
            source,
            url,
            enable_on_complete,
        } => {
            state.overlay = Some(mcp_auth_overlay(kind, &source, &url, enable_on_complete));
            push_local_notice(
                state,
                &format!("Authentication URL for `{source}`:\n\n{url}"),
                EntryStatus::Completed,
            );
        }
        McpEffect::Refresh { kind, source } => {
            let method = match kind {
                IntegrationKind::Connector => "connectors/refresh",
                IntegrationKind::McpServer => "mcp/refresh",
            };
            schedule_ui_call(
                runtime,
                method,
                json!({"name": source}),
                UiOperation::Mcp(McpPendingOperation::Refresh { kind, source }),
                state,
            );
        }
        McpEffect::CompleteAuthentication {
            kind,
            source,
            enable_source,
        } => match kind {
            IntegrationKind::Connector => {
                execute_mcp_effect(McpEffect::Refresh { kind, source }, runtime, state, opener);
            }
            IntegrationKind::McpServer => {
                schedule_ui_call(
                    runtime,
                    "mcp/auth/complete",
                    json!({"name": source}),
                    UiOperation::Mcp(McpPendingOperation::CompleteAuthentication {
                        source,
                        enable_source,
                    }),
                    state,
                );
            }
        },
        McpEffect::Logout { source } => {
            schedule_ui_call(
                runtime,
                "mcp/logout",
                json!({"name": source}),
                UiOperation::Mcp(McpPendingOperation::Logout),
                state,
            );
        }
        McpEffect::Close => state.overlay = None,
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
            state.push_diagnostic(error);
            return;
        }
    };
    match operation {
        McpPendingOperation::AwaitLogin { source, attempt } => {
            schedule_ui_call(
                runtime,
                "mcp/auth/complete",
                json!({"name": source}),
                UiOperation::Mcp(McpPendingOperation::LoginChecked { source, attempt }),
                state,
            );
            return;
        }
        McpPendingOperation::LoginChecked { source, attempt } => {
            let verified = dispatch
                .result
                .get("auth")
                .and_then(|auth| auth.get("verified"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if verified {
                push_local_document(state, mcp_authenticated(&source));
            } else if attempt < super::LOGIN_POLL_ATTEMPTS {
                schedule_ui_external(
                    runtime,
                    UiOperation::Mcp(McpPendingOperation::AwaitLogin {
                        source,
                        attempt: attempt.saturating_add(1),
                    }),
                    async {
                        tokio::time::sleep(super::LOGIN_POLL_INTERVAL).await;
                        Ok(())
                    },
                    state,
                );
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
            // A server login publishes its URL as `mcp/authUrl` and declares
            // nothing on the answer; a connector answers with the URL directly.
            let notified = dispatch
                .notifications
                .iter()
                .find(|notification| notification.method == "mcp/authUrl")
                .and_then(|notification| notification.params.get("url"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
            let url = notified
                .as_deref()
                .or_else(|| value.get("url").and_then(Value::as_str));
            if let Some(url) = url.filter(|url| valid_auth_url(url)) {
                state.overlay = Some(mcp_auth_overlay(kind, &source, url, enable_on_complete));
            } else {
                state.push_diagnostic("Authentication source returned an invalid or unsafe URL");
            }
        }
        McpPendingOperation::Refresh { kind, source } => {
            if kind == IntegrationKind::Connector {
                // The refresh answers with the runtime it produced, so the
                // connector's new state is read off the published source list.
                let connected =
                    sources_of(value.get("runtime").unwrap_or(&Value::Null), "connector")
                        .find(|candidate| {
                            candidate.get("name").and_then(Value::as_str) == Some(&source)
                        })
                        .and_then(|candidate| candidate.get("status"))
                        .and_then(Value::as_str)
                        == Some("connected");
                if !connected {
                    push_local_notice(
                        state,
                        "Connector authentication is still pending",
                        EntryStatus::Streaming,
                    );
                    return;
                }
            }
            schedule_read_integrations(runtime, None, None, state);
        }
        McpPendingOperation::CompleteAuthentication {
            source,
            enable_source,
        } => {
            let verified = value
                .pointer("/auth/verified")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !verified {
                push_local_notice(
                    state,
                    "MCP authentication is still pending",
                    EntryStatus::Streaming,
                );
            } else if enable_source {
                schedule_ui_call(
                    runtime,
                    "mcp/toggle",
                    json!({"name": source, "toolName": null, "disabled": false}),
                    UiOperation::Mcp(McpPendingOperation::EnableAfterAuthentication),
                    state,
                );
            } else {
                schedule_read_integrations(runtime, None, None, state);
            }
        }
        McpPendingOperation::EnableAfterAuthentication | McpPendingOperation::Logout => {
            schedule_read_integrations(runtime, None, None, state);
        }
        McpPendingOperation::SetEnabled { target, detail } => {
            schedule_read_integrations(runtime, None, detail.then_some(target), state);
        }
        // Answered before the dispatch is read as a runtime.
        McpPendingOperation::AwaitLogin { .. } | McpPendingOperation::LoginChecked { .. } => {}
        operation @ (McpPendingOperation::CopyUrl | McpPendingOperation::OpenUrl) => {
            let message = if matches!(operation, McpPendingOperation::CopyUrl) {
                "Authentication URL copied to the clipboard"
            } else {
                "Authentication URL opened in the browser"
            };
            push_local_notice(state, message, EntryStatus::Completed);
        }
    }
}

pub(in crate::tui) fn valid_auth_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        url.scheme() == "https"
            || (url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
                }))
    })
}

pub(super) async fn open_auth_url(url: String) -> Result<(), String> {
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

pub(super) fn refresh_selected_mcp(state: &mut TuiState) -> Option<McpEffect> {
    let (target, detail) = selected_integration(state)?;
    let _ = detail;
    Some(McpEffect::Refresh {
        kind: target.kind,
        source: target.source,
    })
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
    use super::{McpEffect, reduce_auth_action, valid_auth_url};
    use crate::tui::interaction::{AuthAction, AuthActionKind, IntegrationKind};

    #[test]
    fn auth_urls_require_https_or_loopback_http() {
        assert!(valid_auth_url("https://auth.example/authorize"));
        assert!(valid_auth_url("http://localhost:4317/callback"));
        assert!(valid_auth_url("http://127.0.0.1/callback"));
        assert!(!valid_auth_url("http://auth.example/authorize"));
        assert!(!valid_auth_url("file:///tmp/token"));
        assert!(!valid_auth_url("not a URL"));
    }

    #[test]
    fn authentication_actions_reduce_to_explicit_effects() {
        let action = AuthAction {
            kind: IntegrationKind::Connector,
            source: "drive".to_owned(),
            url: "https://auth.example/drive".to_owned(),
            action: AuthActionKind::Copy,
            enable_on_complete: false,
        };
        assert_eq!(
            reduce_auth_action(&action),
            McpEffect::CopyUrl {
                kind: IntegrationKind::Connector,
                source: "drive".to_owned(),
                url: "https://auth.example/drive".to_owned(),
                enable_on_complete: false,
            }
        );
        let completion = AuthAction {
            action: AuthActionKind::Refresh,
            ..action
        };
        assert_eq!(
            reduce_auth_action(&completion),
            McpEffect::CompleteAuthentication {
                kind: IntegrationKind::Connector,
                source: "drive".to_owned(),
                enable_source: false,
            }
        );
    }
}
