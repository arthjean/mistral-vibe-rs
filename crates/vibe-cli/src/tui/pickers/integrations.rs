use serde::Deserialize;
use serde_json::Value;
use vibe_app_server::vocabulary::McpSourceStatus;

use super::super::interaction::{
    AuthAction, AuthActionKind, IntegrationKind, IntegrationTarget, Overlay, OverlayAction,
    OverlayItem, OverlayKind, QueryFilter,
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
    /// Why the source failed to bootstrap, which its detail view shows in
    /// place of its tools.
    #[serde(default)]
    pub error: Option<String>,
    /// The plugin that ships the source, which its row names.
    #[serde(rename = "pluginName", default)]
    pub plugin_name: Option<String>,
    /// What the last discovery of the source reported, read off the state's
    /// `discoveryErrors` (reference `MCPState.discovery_errors`).
    #[serde(skip)]
    pub discovery_error: Option<String>,
}

/// Reference `MCPToolSummary`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct McpTool {
    pub name: String,
    pub description: String,
    pub enabled: bool,
}

/// One source of a session's accepted connector catalog (reference
/// `SessionConnectorSourceView`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CatalogSource {
    alias: String,
    display_name: String,
    status: McpSourceStatus,
    #[serde(default)]
    tools: Vec<CatalogTool>,
    error: Option<String>,
}

/// Reference `SessionConnectorToolView`.
#[derive(Deserialize)]
struct CatalogTool {
    name: String,
    description: Option<String>,
    enabled: bool,
}

/// Reference `mcp_app._source_status`: an unavailable server points at its
/// configuration and an unavailable connector at a refresh.
fn status_label(status: McpSourceStatus, kind: IntegrationKind) -> &'static str {
    match status {
        McpSourceStatus::Disabled => "disabled",
        McpSourceStatus::Connected => "connected",
        McpSourceStatus::Enabled => "enabled",
        McpSourceStatus::NeedsAuth => "needs auth",
        McpSourceStatus::NeedsSetup => "needs setup",
        McpSourceStatus::Unavailable => match kind {
            IntegrationKind::McpServer => "error - check your config",
            IntegrationKind::Connector => "error - try refreshing",
        },
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
        let discovery_errors = state.pointer("/mcp/discoveryErrors");
        for source in &mut sources {
            if source.display_name.is_empty() {
                source.display_name.clone_from(&source.name);
            }
            source.discovery_error = discovery_errors
                .and_then(|errors| errors.get(&source.name))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned);
        }
        Ok(sources)
    }

    /// Reference `MCPResource.read`: the servers of `sources` and, in place of
    /// its connectors, the ones `connector_catalog/read` answered for the
    /// session, each labeled by the display name its catalog bootstrapped. A
    /// catalog read that failed (`None`) keeps the published connectors, and
    /// an answer without a session lists none.
    ///
    /// # Errors
    ///
    /// Returns the decoding error when the session's sources do not have the
    /// published shape.
    pub fn with_catalog_connectors(
        mut sources: Vec<Self>,
        catalog: Option<&Value>,
    ) -> Result<Vec<Self>, String> {
        let Some(catalog) = catalog else {
            return Ok(sources);
        };
        let connectors = catalog
            .pointer("/session/sources")
            .map_or_else(|| Ok(Vec::new()), Vec::<CatalogSource>::deserialize)
            .map_err(|error| format!("Malformed connector catalog: {error}"))?;
        sources.retain(|source| source.kind == IntegrationKind::McpServer);
        sources.extend(connectors.into_iter().map(|connector| {
            Self {
                name: connector.alias,
                display_name: connector.display_name,
                kind: IntegrationKind::Connector,
                transport: "connector".to_owned(),
                status: connector.status,
                tools: connector
                    .tools
                    .into_iter()
                    .map(|tool| McpTool {
                        name: tool.name,
                        description: tool.description.unwrap_or_default(),
                        enabled: tool.enabled,
                    })
                    .collect(),
                error: connector.error,
                plugin_name: None,
                discovery_error: None,
            }
        }));
        Ok(sources)
    }

    /// Reference `_awaits_auth`: a source that failed to bootstrap opens on
    /// its error rather than on the sign-in.
    fn awaits_auth(&self) -> bool {
        self.status == McpSourceStatus::NeedsAuth && self.error.is_none()
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
    let mut overlay = Overlay::new(OverlayKind::Mcp, title, items);
    overlay.filter = QueryFilter::FuzzyGroups;
    overlay
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
    let row = |id: &str, label: &str, description: &str| {
        OverlayItem::new(id, label, description, true)
            .with_action(OverlayAction::Integration(target.clone()))
    };
    let mut items = if let Some(error) = source.and_then(|source| source.error.as_deref()) {
        vec![
            row("mcp-detail:bootstrap", "Failed to bootstrap", ""),
            row("mcp-detail:error", error, ""),
        ]
    } else if status == McpSourceStatus::NeedsSetup {
        // The browser's `r` binding performs the refresh the instruction names.
        vec![row(
            "mcp-detail:setup",
            "Set up credentials in the Mistral dashboard, then press r to refresh.",
            "",
        )]
    } else {
        let mut tools = source
            .into_iter()
            .flat_map(|source| {
                source.tools.iter().map(move |tool| {
                    integration_item(
                        source,
                        Some(&tool.name),
                        tool.enabled,
                        &tool.name,
                        tool_description(&tool.description, tool.enabled),
                    )
                })
            })
            .collect::<Vec<_>>();
        tools.sort_by_key(|item| item.label.to_ascii_lowercase());
        tools
    };
    if items.is_empty() {
        match source.filter(|source| failed_discovery(source)) {
            Some(source) => {
                items.push(row(
                    "mcp-detail:discovery",
                    "Tool discovery failed",
                    "Backspace returns to all sources",
                ));
                if let Some(error) = &source.discovery_error {
                    items.push(row("mcp-detail:discovery-error", error, ""));
                }
            }
            None => items.push(row(
                "mcp-detail:empty",
                "No tools discovered",
                "Backspace returns to all sources",
            )),
        }
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

/// A server that is unavailable without a tool, which reference
/// `_add_source_group` and `_show_detail_view` report as a failed discovery.
fn failed_discovery(source: &McpSource) -> bool {
    source.kind == IntegrationKind::McpServer
        && source.status == McpSourceStatus::Unavailable
        && source.tools.is_empty()
}

/// Reference `_add_source_group`: the transport, the plugin owning the source
/// when there is one, the tools, and the status.
fn source_item(source: &McpSource) -> OverlayItem {
    let tools = if failed_discovery(source) {
        "tool discovery failed".to_owned()
    } else {
        let enabled = source.tools.iter().filter(|tool| tool.enabled).count();
        tool_count_text(enabled, source.tools.len())
    };
    let owner = source
        .plugin_name
        .as_ref()
        .map(|plugin| format!("plugin:{plugin} · "))
        .unwrap_or_default();
    integration_item(
        source,
        None,
        source.status != McpSourceStatus::Disabled,
        &source.display_name,
        format!(
            "{} · {owner}{tools} · {}",
            source.transport,
            status_label(source.status, source.kind),
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
    source: &McpSource,
    tool: Option<&str>,
    enabled: bool,
    label: impl Into<String>,
    description: impl Into<String>,
) -> OverlayItem {
    let kind_id = match source.kind {
        IntegrationKind::McpServer => "mcp",
        IntegrationKind::Connector => "connector",
    };
    let id = tool.map_or_else(
        || format!("integration:{kind_id}:{}", source.name),
        |tool| format!("integration:{kind_id}:{}:{tool}", source.name),
    );
    OverlayItem::new(id, label, description, false).with_action(OverlayAction::Integration(
        IntegrationTarget {
            kind: source.kind,
            source: source.name.clone(),
            tool: tool.map(ToOwned::to_owned),
            enabled,
            requires_auth: source.awaits_auth(),
            requires_setup: source.status == McpSourceStatus::NeedsSetup,
            plugin_name: source.plugin_name.clone(),
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
