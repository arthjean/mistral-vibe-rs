use serde::Deserialize;
use serde_json::Value;

use super::super::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, IntegrationTarget, Overlay, OverlayAction,
    OverlayItem, OverlayKind,
};

/// One source of the published MCP state, servers and connectors in one list
/// as `mcp/read` answers it (reference `MCPSourceSummary`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpSource {
    pub name: String,
    /// The label a row and a detail title show, `name` staying the key every
    /// toggle and row id uses. A producer that publishes none labels the
    /// source by its name, as the reference model's validator does.
    #[serde(rename = "displayName", default)]
    pub display_name: String,
    pub kind: IntegrationKind,
    pub transport: String,
    pub status: McpSourceStatus,
    pub tools: Vec<McpTool>,
}

/// Reference `MCPToolSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

/// Reference `MCPSourceStatus`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSourceStatus {
    Disabled,
    Connected,
    Enabled,
    NeedsAuth,
    NeedsSetup,
    Unavailable,
}

impl McpSourceStatus {
    /// The wire word, which `/mcp status` prints verbatim.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connected => "connected",
            Self::Enabled => "enabled",
            Self::NeedsAuth => "needs_auth",
            Self::NeedsSetup => "needs_setup",
            Self::Unavailable => "unavailable",
        }
    }

    /// Reference `mcp_app._source_status`: an unavailable server points at
    /// its configuration and an unavailable connector at a refresh.
    fn label(self, kind: IntegrationKind) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connected => "connected",
            Self::Enabled => "enabled",
            Self::NeedsAuth => "needs auth",
            Self::NeedsSetup => "needs setup",
            Self::Unavailable => match kind {
                IntegrationKind::McpServer => "error - check your config",
                IntegrationKind::Connector => "error - try refreshing",
            },
        }
    }
}

impl McpSource {
    /// The sources of a published MCP state: `mcp/read` answers it at the top
    /// level and `connectors/refresh` under `runtime`. A payload without the
    /// list publishes no source.
    ///
    /// # Errors
    ///
    /// Returns the decoding error when the list does not have the published shape.
    pub fn published(state: &Value) -> Result<Vec<Self>, String> {
        let mut sources = state
            .pointer("/mcp/sources")
            .map_or_else(|| Ok(Vec::new()), Vec::<Self>::deserialize)
            .map_err(|error| format!("Malformed MCP state: {error}"))?;
        for source in &mut sources {
            if source.display_name.is_empty() {
                source.display_name.clone_from(&source.name);
            }
        }
        Ok(sources)
    }
}

#[must_use]
pub fn mcp_overlay(sources: &[McpSource]) -> Overlay {
    let mut items = Vec::new();
    let servers = sorted_sources(sources, IntegrationKind::McpServer);
    if !servers.is_empty() {
        items.push(OverlayItem::new(
            "heading:mcp",
            "Local MCP Servers",
            "",
            true,
        ));
    }
    items.extend(servers.into_iter().map(source_item));
    let connectors = sorted_sources(sources, IntegrationKind::Connector);
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
    items.extend(connectors.into_iter().map(source_item));
    let title = if has_connectors {
        "MCP Servers & Connectors"
    } else {
        "MCP Servers"
    };
    Overlay::new(OverlayKind::Mcp, title, items)
}

/// Reference `MCPApp._show_detail_view`: a source's tools, or the one
/// instruction a source that needs setup gives instead.
#[must_use]
pub fn mcp_detail_overlay(sources: &[McpSource], target: &IntegrationTarget) -> Overlay {
    let prefix = match target.kind {
        IntegrationKind::McpServer => "MCP Server",
        IntegrationKind::Connector => "Connector",
    };
    let source = sources
        .iter()
        .find(|source| source.kind == target.kind && source.name == target.source);
    let title = format!(
        "{prefix}: {}",
        source.map_or(target.source.as_str(), |source| source
            .display_name
            .as_str())
    );
    let status = source.map_or(McpSourceStatus::Unavailable, |source| source.status);
    let mut items = if status == McpSourceStatus::NeedsSetup {
        // The browser's `r` binding performs the refresh the instruction names.
        vec![
            OverlayItem::new(
                "mcp-detail:setup",
                "Set up credentials in the Mistral dashboard, then press r to refresh.",
                "",
                true,
            )
            .with_action(OverlayAction::Integration(target.clone())),
        ]
    } else {
        let mut tools = source
            .into_iter()
            .flat_map(|source| &source.tools)
            .map(|tool| {
                integration_item(
                    target.kind,
                    &target.source,
                    Some(&tool.name),
                    tool.enabled,
                    status,
                    &tool.name,
                    tool_description(&tool.description, tool.enabled),
                )
            })
            .collect::<Vec<_>>();
        tools.sort_by_key(|item| item.label.to_ascii_lowercase());
        tools
    };
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

/// The sources of one kind, the ones with tools first and each group by the
/// label it shows, its name breaking ties (reference `_sort_sources_for_menu`).
fn sorted_sources(sources: &[McpSource], kind: IntegrationKind) -> Vec<&McpSource> {
    let mut sources = sources
        .iter()
        .filter(|source| source.kind == kind)
        .collect::<Vec<_>>();
    sources.sort_by_key(|source| {
        (
            source.tools.is_empty(),
            source.display_name.to_lowercase(),
            source.name.as_str(),
        )
    });
    sources
}

fn source_item(source: &McpSource) -> OverlayItem {
    let enabled = source.tools.iter().filter(|tool| tool.enabled).count();
    let status = source.status.label(source.kind);
    integration_item(
        source.kind,
        &source.name,
        None,
        source.status != McpSourceStatus::Disabled,
        source.status,
        &source.display_name,
        format!(
            "{} · {} · {status}",
            source.transport,
            tool_count_text(enabled, source.tools.len()),
        ),
    )
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

/// The reference marks a disabled tool with a trailing `(disabled)` tag instead of
/// a leading state token, so both surfaces present the same sentence.
fn tool_description(description: &str, enabled: bool) -> String {
    match (enabled, description.is_empty()) {
        (true, _) => description.to_owned(),
        (false, true) => "(disabled)".to_owned(),
        (false, false) => format!("{description} (disabled)"),
    }
}

fn integration_item(
    kind: IntegrationKind,
    source: &str,
    tool: Option<&str>,
    enabled: bool,
    status: McpSourceStatus,
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
            requires_auth: status == McpSourceStatus::NeedsAuth,
            requires_setup: status == McpSourceStatus::NeedsSetup,
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

#[cfg(test)]
mod integrations_tests;
