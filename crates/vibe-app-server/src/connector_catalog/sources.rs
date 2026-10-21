//! How a session publishes the catalog it accepted: its connector state, its
//! counts, and the MCP rows of the legacy and Unified Harness runtimes.
//!
//! Every status, here and in the catalog view, comes from
//! [`SourceStatus::derive`]; each projection only says what enables a source,
//! what connects it and which auth action can hold it, as its reference
//! function reads them.

use serde_json::{Value, json};

use super::{
    ConnectorSelection, ConnectorSetting, ResolvedCatalog, ResolvedConnector, SessionConnectors,
};

/// How a connector stands where a session publishes it (reference
/// `MCPSourceStatus` and the `status` of `SessionConnectorSourceState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SourceStatus {
    Disabled,
    Connected,
    NeedsAuth,
    NeedsSetup,
    Unavailable,
}

impl SourceStatus {
    /// A source that is not enabled is disabled, one that is connected is
    /// connected, and any other waits on the auth action holding it:
    /// authorization, credentials, or nothing it can act on.
    pub(super) fn derive(enabled: bool, connected: bool, held_by: Option<&str>) -> Self {
        if !enabled {
            Self::Disabled
        } else if connected {
            Self::Connected
        } else {
            match held_by {
                Some("oauth") => Self::NeedsAuth,
                Some("credentials_setup") => Self::NeedsSetup,
                _ => Self::Unavailable,
            }
        }
    }

    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Connected => "connected",
            Self::NeedsAuth => "needs_auth",
            Self::NeedsSetup => "needs_setup",
            Self::Unavailable => "unavailable",
        }
    }
}

/// Reference `_legacy_connector_source`: how a session publishes one
/// accepted connector. A ready connector publishing a tool is one the
/// session's registry connected (reference `ConnectorRegistry.is_connected`),
/// unless its auth action is unknown; only a connector that is not ready is
/// held by its auth action, so a ready one left unconnected is unavailable.
fn legacy_status(connector: &ResolvedConnector, selection: &ConnectorSelection) -> SourceStatus {
    SourceStatus::derive(
        selection.source_enabled(&connector.alias),
        connector.ready && connector.auth_action != "unknown" && !connector.tools.is_empty(),
        (!connector.ready).then_some(connector.auth_action),
    )
}

impl SessionConnectors {
    fn route_revision_text(&self) -> String {
        format!("legacy-connector-routes:{}", self.route_revision)
    }

    /// Reference `LegacySessionBackend._session_connector_state` projected
    /// as `SessionConnectorStateView`. A connector the catalog marks ready is
    /// one the session connected.
    pub(crate) fn view(&self) -> Value {
        let Some((catalog, selection)) = &self.accepted else {
            return json!({
                "acceptedCatalogRevision": "",
                "acceptedSelectionRevision": "",
                "routeRevision": self.route_revision_text(),
                "sources": [],
            });
        };
        let sources = catalog
            .connectors
            .iter()
            .map(|connector| {
                json!({
                    "alias": connector.alias,
                    "displayName": connector.display_name,
                    "status": legacy_status(connector, selection).as_str(),
                    "tools": connector.tools.iter().map(|tool| json!({
                        "name": tool.raw_name,
                        "description": tool.description,
                        "enabled": selection.tool_enabled(&connector.alias, &tool.raw_name),
                    })).collect::<Vec<_>>(),
                    "error": connector.diagnostic(),
                })
            })
            .collect::<Vec<_>>();
        json!({
            "acceptedCatalogRevision": catalog.revision,
            "acceptedSelectionRevision": selection.revision,
            "routeRevision": self.route_revision_text(),
            "sources": sources,
        })
    }

    /// The source `alias` publishes, with the connector behind it.
    pub(crate) fn source(&self, alias: &str) -> Option<(&ResolvedConnector, &'static str)> {
        let (catalog, selection) = self.accepted.as_ref()?;
        let connector = catalog
            .connectors
            .iter()
            .find(|connector| connector.alias == alias)?;
        Some((connector, legacy_status(connector, selection).as_str()))
    }

    /// Reference `project_connector_counts` for the runtime: the counts over
    /// the accepted sources, with no total for a session without a registry.
    pub(crate) fn runtime_counts(&self) -> Value {
        let mut counts = self.counts();
        if !self.registry {
            counts["total"] = Value::Null;
        }
        counts
    }

    /// Reference `ConnectorCounts` over the accepted sources.
    pub(crate) fn counts(&self) -> Value {
        let (connected, total) = self
            .accepted
            .as_ref()
            .map_or((0, 0), |(catalog, selection)| {
                (
                    catalog
                        .connectors
                        .iter()
                        .filter(|connector| {
                            legacy_status(connector, selection) == SourceStatus::Connected
                        })
                        .count(),
                    catalog.connectors.len(),
                )
            });
        json!({"connected": connected, "total": total})
    }

    /// Reference `_update_connector_projection` on a Unified Harness session:
    /// one row per connector of the accepted catalog, in alias order, with
    /// the core's own status (reference `_source_status` in
    /// `_connector_naming.py`: a ready connector is connected) and each tool
    /// in name order, enabled when the connector is connected and the
    /// selection keeps it. Answers the rows with the counts over them.
    pub(crate) fn unified_sources(&self) -> (Vec<Value>, Value) {
        let Some((catalog, selection)) = &self.accepted else {
            return (Vec::new(), json!({"connected": 0, "total": 0}));
        };
        let mut connectors = catalog.connectors.iter().collect::<Vec<_>>();
        connectors
            .sort_by(|left, right| (&left.alias, &left.raw_id).cmp(&(&right.alias, &right.raw_id)));
        let mut rows = Vec::with_capacity(connectors.len());
        let mut connected = 0;
        for connector in connectors {
            let status = SourceStatus::derive(
                selection.source_enabled(&connector.alias),
                connector.ready,
                Some(connector.auth_action),
            );
            connected += usize::from(status == SourceStatus::Connected);
            let mut tools = connector.tools.iter().collect::<Vec<_>>();
            tools.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
            let tools = tools
                .into_iter()
                .map(|tool| {
                    json!({
                        "name": tool.raw_name,
                        "description": tool
                            .description
                            .clone()
                            .filter(|description| !description.is_empty())
                            .unwrap_or_else(|| format!("Connector tool {}.", tool.raw_name)),
                        "enabled": status == SourceStatus::Connected
                            && selection.tool_enabled(&connector.alias, &tool.raw_name),
                    })
                })
                .collect::<Vec<_>>();
            rows.push(json!({
                "name": connector.alias,
                "displayName": connector.display_name,
                "kind": "connector",
                "transport": "connector",
                "status": status.as_str(),
                "tools": tools,
                "error": connector.diagnostic(),
                "pluginName": null,
            }));
        }
        let total = rows.len();
        (rows, json!({"connected": connected, "total": total}))
    }

    /// Reference `_project_mcp_connectors` once the legacy backend handed the
    /// accepted catalog to the session's registry: one row per connector the
    /// configuration names or the catalog publishes, sorted by name. A
    /// session that accepted no catalog lists the configured names alone.
    ///
    /// A connector no entry names, or one its entry disables, is disabled; a
    /// ready one publishing at least one tool is connected, and any other is
    /// held by its auth action whether or not it is ready. Its published
    /// tools are listed with the selection deciding which are enabled, and a
    /// connector that is not connected lists its catalog tools, all disabled.
    pub(crate) fn mcp_sources(&self, settings: &[ConnectorSetting]) -> Vec<Value> {
        let empty = Vec::new();
        let (connectors, selection) = self
            .accepted
            .as_ref()
            .map_or((&empty, None), |(catalog, selection)| {
                (&catalog.connectors, Some(selection))
            });
        let mut names = settings
            .iter()
            .map(|setting| setting.alias.as_str())
            .chain(connectors.iter().map(|connector| connector.alias.as_str()))
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        names
            .into_iter()
            .map(|name| {
                let setting = settings.iter().find(|setting| setting.alias == name);
                let connector = connectors.iter().find(|connector| connector.alias == name);
                let connected = connector
                    .is_some_and(|connector| connector.ready && !connector.tools.is_empty());
                let status = SourceStatus::derive(
                    setting.is_some_and(|setting| !setting.disabled),
                    connected,
                    connector.map(|connector| connector.auth_action),
                );
                let mut tools = connector
                    .map(|connector| connector.tools.iter().collect::<Vec<_>>())
                    .unwrap_or_default();
                tools.sort_by(|left, right| left.raw_name.cmp(&right.raw_name));
                let tools = tools
                    .into_iter()
                    .map(|tool| {
                        let (description, enabled) = if connected {
                            let description = tool
                                .description
                                .clone()
                                .unwrap_or_else(|| format!("Connector tool '{}'", tool.raw_name));
                            let first = description
                                .split('\n')
                                .next()
                                .unwrap_or_default()
                                .trim()
                                .to_owned();
                            let enabled = selection
                                .is_some_and(|selection| selection.tool_enabled(name, &tool.raw_name));
                            (first, enabled)
                        } else {
                            (tool.description.clone().unwrap_or_default(), false)
                        };
                        json!({"name": tool.raw_name, "description": description, "enabled": enabled})
                    })
                    .collect::<Vec<_>>();
                json!({
                    "name": name,
                    "displayName": name,
                    "kind": "connector",
                    "transport": "connector",
                    "status": status.as_str(),
                    "tools": tools,
                    "error": connector.and_then(|connector| connector.diagnostic()),
                    "pluginName": null,
                })
            })
            .collect()
    }

    /// Accepts a catalog and the selection over it, which reroutes the
    /// session's connectors.
    pub(crate) fn accept(
        &mut self,
        catalog: ResolvedCatalog,
        selection: ConnectorSelection,
    ) -> Value {
        self.accepted = Some((catalog, selection));
        self.load_error = None;
        self.route_revision = self.route_revision.saturating_add(1);
        self.view()
    }
}
