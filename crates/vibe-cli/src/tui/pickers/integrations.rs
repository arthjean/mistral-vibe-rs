use serde_json::Value;

use super::super::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, IntegrationTarget, Overlay, OverlayAction,
    OverlayItem, OverlayKind,
};

/// Reference vocabulary: `mcp_app._source_status` renders `UNAVAILABLE` this way,
/// and connectors are presented with a `connector` transport tag.
const UNAVAILABLE_STATUS: &str = "error - try refreshing";
const CONNECTOR_TRANSPORT: &str = "connector";

#[must_use]
pub fn mcp_overlay(mcp_result: &Value, connector_result: &Value) -> Overlay {
    let mut items = Vec::new();
    let mut servers = mcp_result
        .pointer("/mcp/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    servers.sort_by_key(|source| {
        let has_tools = source
            .get("tools")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        let name = source
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        (!has_tools, name.to_ascii_lowercase(), name)
    });
    if !servers.is_empty() {
        items.push(OverlayItem::new(
            "heading:mcp",
            "Local MCP Servers",
            "",
            true,
        ));
    }
    for source in servers {
        if let Some(item) = mcp_source_item(source, IntegrationKind::McpServer) {
            items.push(item);
        }
    }
    let mut connectors = connector_result
        .pointer("/connectors/sources")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    connectors.sort_by_key(|source| {
        let has_tools = source
            .get("toolNames")
            .and_then(Value::as_array)
            .is_some_and(|tools| !tools.is_empty());
        let name = source
            .get("name")
            .or_else(|| source.get("alias"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        (!has_tools, name.to_ascii_lowercase(), name)
    });
    let has_connectors = !connectors.is_empty();
    if has_connectors {
        if !items.is_empty() {
            items.push(OverlayItem::new("gap:connectors", "", "", true));
        }
        items.push(OverlayItem::new(
            "heading:connectors",
            "Available Connectors",
            "",
            true,
        ));
    }
    for source in connectors {
        let id = source
            .get("id")
            .or_else(|| source.get("name"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let label = source
            .get("name")
            .or_else(|| source.get("alias"))
            .and_then(Value::as_str)
            .unwrap_or(id);
        let status = connector_status(source);
        let source_enabled = source
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let tool_names = source
            .get("toolNames")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let disabled = source
            .get("disabledTools")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let total = tool_names.len();
        let enabled = tool_names
            .iter()
            .filter(|tool| !disabled.contains(tool))
            .count();
        items.push(integration_item(
            IntegrationKind::Connector,
            id,
            None,
            source_enabled,
            status,
            label,
            format!(
                "{CONNECTOR_TRANSPORT} · {} · {status}",
                tool_count_text(enabled, total),
            ),
        ));
    }
    let title = if has_connectors {
        "MCP Servers & Connectors"
    } else {
        "MCP Servers"
    };
    Overlay::new(OverlayKind::Mcp, title, items)
}

#[must_use]
pub fn mcp_detail_overlay(
    mcp_result: &Value,
    connector_result: &Value,
    target: &IntegrationTarget,
) -> Overlay {
    let (title, mut items, requires_setup) = match target.kind {
        IntegrationKind::McpServer => {
            let source = mcp_result
                .pointer("/mcp/sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|source| source.get("name").and_then(Value::as_str) == Some(&target.source));
            let mut tools = source
                .and_then(|source| source.get("tools"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|tool| mcp_tool_item(tool, target.kind, &target.source))
                .collect::<Vec<_>>();
            tools.sort_by_key(|item| item.label.to_ascii_lowercase());
            (format!("MCP Server: {}", target.source), tools, false)
        }
        IntegrationKind::Connector => {
            let source = connector_result
                .pointer("/connectors/sources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .find(|source| {
                    source
                        .get("id")
                        .or_else(|| source.get("alias"))
                        .or_else(|| source.get("name"))
                        .and_then(Value::as_str)
                        == Some(&target.source)
                });
            let status = source.map_or(UNAVAILABLE_STATUS, connector_status);
            let disabled = source
                .and_then(|source| source.get("disabledTools"))
                .and_then(Value::as_array);
            let mut tools = source
                .and_then(|source| source.get("toolNames"))
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|name| {
                    let enabled = !disabled.is_some_and(|tools| {
                        tools
                            .iter()
                            .any(|candidate| candidate.as_str() == Some(name))
                    });
                    integration_item(
                        target.kind,
                        &target.source,
                        Some(name),
                        enabled,
                        status,
                        name,
                        tool_description("", enabled),
                    )
                })
                .collect::<Vec<_>>();
            tools.sort_by_key(|item| item.label.to_ascii_lowercase());
            let label = source
                .and_then(|source| source.get("name").or_else(|| source.get("alias")))
                .and_then(Value::as_str)
                .unwrap_or(&target.source);
            (
                format!("Connector: {label}"),
                tools,
                status == "needs setup",
            )
        }
    };
    if requires_setup {
        // Reference `MCPApp._refresh_view` for `NEEDS_SETUP`: one instruction,
        // and the browser's `r` binding performs the refresh it names.
        items = vec![
            OverlayItem::new(
                "mcp-detail:setup",
                "Set up credentials in the Mistral dashboard, then press r to refresh.",
                "",
                true,
            )
            .with_action(OverlayAction::Integration(target.clone())),
        ];
    }
    if items.is_empty() {
        items.push(
            OverlayItem::new(
                "mcp-detail:empty",
                "No tools discovered",
                "Backspace returns to all sources",
                true,
            )
            .with_action(OverlayAction::Integration(target.clone())),
        );
    }
    Overlay::new(OverlayKind::McpDetail, title, items)
}

fn mcp_source_item(source: &Value, kind: IntegrationKind) -> Option<OverlayItem> {
    let name = source.get("name").and_then(Value::as_str)?;
    let raw_status = source
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("healthy");
    let enabled = source
        .get("enabled")
        .and_then(Value::as_bool)
        .unwrap_or(raw_status != "disabled");
    let status = if enabled {
        match raw_status {
            "connected" => "connected",
            "needs_auth" => "needs auth",
            "needs_setup" => "needs setup",
            "unavailable" => UNAVAILABLE_STATUS,
            _ => "enabled",
        }
    } else {
        "disabled"
    };
    let tools = source
        .get("tools")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let total = tools.len();
    let enabled_tools = tools
        .iter()
        .filter(|tool| tool.get("enabled").and_then(Value::as_bool).unwrap_or(true))
        .count();
    Some(integration_item(
        kind,
        name,
        None,
        enabled,
        status,
        name,
        format!(
            "{} · {} · {status}",
            source
                .get("transport")
                .and_then(Value::as_str)
                .unwrap_or("unknown"),
            tool_count_text(enabled_tools, total),
        ),
    ))
}

fn tool_count_text(enabled: usize, total: usize) -> String {
    if enabled < total {
        format!(
            "{enabled}/{total} {}",
            if total == 1 { "tool" } else { "tools" }
        )
    } else if enabled == 0 {
        "no tools".to_owned()
    } else {
        format!("{enabled} {}", if enabled == 1 { "tool" } else { "tools" })
    }
}

fn mcp_tool_item(tool: &Value, kind: IntegrationKind, source: &str) -> Option<OverlayItem> {
    let name = tool.get("name").and_then(Value::as_str)?;
    let enabled = tool.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    let description = tool
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or_default();
    Some(integration_item(
        kind,
        source,
        Some(name),
        enabled,
        if enabled { "enabled" } else { "disabled" },
        name,
        tool_description(description, enabled),
    ))
}

/// The reference marks a disabled tool with a trailing `(disabled)` tag instead of
/// a leading state token, so both surfaces present the same sentence.
fn tool_description(description: &str, enabled: bool) -> String {
    match (enabled, description.is_empty()) {
        (true, _) => description.to_owned(),
        (false, true) => "(disabled)".to_owned(),
        (false, false) => format!("{description} (disabled)"),
    }
}

fn connector_status(source: &Value) -> &'static str {
    if source.get("enabled").and_then(Value::as_bool) == Some(false) {
        return "disabled";
    }
    match source.get("authState").and_then(Value::as_str) {
        Some("connected" | "not_required") => "connected",
        Some("disconnected") => "needs auth",
        Some("setup_required") => "needs setup",
        _ => UNAVAILABLE_STATUS,
    }
}

fn integration_item(
    kind: IntegrationKind,
    source: &str,
    tool: Option<&str>,
    enabled: bool,
    status: &str,
    label: impl Into<String>,
    description: impl Into<String>,
) -> OverlayItem {
    let kind_id = match kind {
        IntegrationKind::McpServer => "mcp",
        IntegrationKind::Connector => "connector",
    };
    let id = tool.map_or_else(
        || format!("integration:{kind_id}:{source}"),
        |tool| format!("integration:{kind_id}:{source}:{tool}"),
    );
    OverlayItem::new(id, label, description, false).with_action(OverlayAction::Integration(
        IntegrationTarget {
            kind,
            source: source.to_owned(),
            tool: tool.map(ToOwned::to_owned),
            enabled,
            requires_auth: status == "needs auth",
            requires_setup: status == "needs setup",
        },
    ))
}

/// Reference `MCPOAuthApp` once its login published a URL, and
/// `ConnectorAuthApp` once it fetched one: a heading row, a spacer, the three
/// URL rows, and the instruction below them, which carries the URL itself
/// while "Manually show the URL" has it shown.
#[must_use]
pub fn mcp_auth_overlay(
    kind: IntegrationKind,
    source: &str,
    url: &str,
    enable_on_complete: bool,
    url_visible: bool,
) -> Overlay {
    let context = |action: AuthActionKind| {
        OverlayAction::Authenticate(AuthAction {
            kind,
            source: source.to_owned(),
            url: url.to_owned(),
            action,
            enable_on_complete,
            url_visible,
        })
    };
    let (title, heading, open, instruction) = match kind {
        IntegrationKind::McpServer => (
            format!("MCP Server: {source}"),
            "This MCP server requires authentication",
            "Press enter to open auth in your browser",
            "Once authenticated in your browser, return to Vibe",
        ),
        IntegrationKind::Connector => (
            format!("Connector: {source}"),
            "This connector requires authentication",
            "Press Enter to open auth in your browser",
            "Once authenticated, press r to refresh",
        ),
    };
    let row = |id: &str, label: &str, action: AuthActionKind| {
        OverlayItem::new(format!("auth:{id}"), label, "", false).with_action(context(action))
    };
    let items = vec![
        OverlayItem::new("auth:heading", heading, "", true)
            .with_action(context(AuthActionKind::Refresh)),
        OverlayItem::new("auth:spacer", "", "", true),
        row("open", open, AuthActionKind::Open),
        row("copy", "Copy URL to clipboard", AuthActionKind::Copy),
        row("show", "Manually show the URL", AuthActionKind::Show),
    ];
    let mut overlay = Overlay::new(OverlayKind::McpAuth, title, items);
    overlay.notice = Some(if url_visible {
        format!("{url}\n\n{instruction}")
    } else {
        instruction.to_owned()
    });
    overlay
}

/// Reference `MCPOAuthApp._on_login_failed`: the rows give way to one
/// instruction, and `r` starts the login again.
#[must_use]
pub fn mcp_auth_failed_overlay(source: &str, enable_on_complete: bool) -> Overlay {
    Overlay::new(
        OverlayKind::McpAuth,
        format!("MCP Server: {source}"),
        vec![
            OverlayItem::new(
                "auth:failed",
                "Authentication failed. Press R to retry.",
                "",
                true,
            )
            .with_action(OverlayAction::Authenticate(AuthAction {
                kind: IntegrationKind::McpServer,
                source: source.to_owned(),
                url: String::new(),
                action: AuthActionKind::Refresh,
                enable_on_complete,
                url_visible: false,
            })),
        ],
    )
}
