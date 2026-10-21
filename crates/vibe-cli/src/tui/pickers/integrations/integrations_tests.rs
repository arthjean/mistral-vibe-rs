use serde_json::json;

use super::*;

fn published() -> Vec<McpSource> {
    McpSource::published(&json!({"mcp": {"sources": [
        {"name": "zeta", "displayName": "Alpha Docs", "kind": "connector",
         "transport": "connector", "status": "unavailable", "tools": []},
        {"name": "alpha", "displayName": "Zulu Mail", "kind": "connector",
         "transport": "connector", "status": "connected", "tools": []},
        {"name": "local", "kind": "server", "transport": "stdio",
         "status": "unavailable", "tools": []},
    ]}}))
    .expect("the sources decode")
}

/// Reference `MCPSourceSummary._default_display_name`: a source published
/// without a label shows its name.
#[test]
fn a_source_without_a_display_name_shows_its_name() {
    let sources = published();
    assert_eq!(sources[0].display_name, "Alpha Docs");
    assert_eq!(sources[2].display_name, "local");
}

/// Reference `_add_source_group` and `_sort_sources_for_menu`: rows show and
/// sort by the display name while their ids keep the name, and an unavailable
/// source's hint depends on its kind.
#[test]
fn rows_show_and_sort_by_the_display_name() {
    let overlay = mcp_overlay(&published());
    let rows = overlay
        .items
        .iter()
        .filter(|item| !item.disabled)
        .map(|item| {
            (
                item.id.as_str(),
                item.label.as_str(),
                item.description.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            (
                "integration:mcp:local",
                "local",
                "stdio · tool discovery failed · error - check your config"
            ),
            (
                "integration:connector:zeta",
                "Alpha Docs",
                "connector · no tools · error - try refreshing"
            ),
            (
                "integration:connector:alpha",
                "Zulu Mail",
                "connector · no tools · connected"
            ),
        ]
    );
}

/// Reference `_show_detail_view`: the title names the source by its display
/// name.
#[test]
fn the_detail_title_uses_the_display_name() {
    let target = IntegrationTarget {
        kind: IntegrationKind::Connector,
        source: "zeta".to_owned(),
        tool: None,
        enabled: true,
        requires_auth: false,
        requires_setup: false,
        plugin_name: None,
    };
    assert_eq!(
        mcp_detail_overlay(&published(), &target).title,
        "Connector: Alpha Docs"
    );
}

fn server_target(source: &str) -> IntegrationTarget {
    IntegrationTarget {
        kind: IntegrationKind::McpServer,
        source: source.to_owned(),
        tool: None,
        enabled: true,
        requires_auth: false,
        requires_setup: false,
        plugin_name: None,
    }
}

fn detail_rows(overlay: &Overlay) -> Vec<(&str, bool)> {
    overlay
        .items
        .iter()
        .map(|item| (item.label.as_str(), item.disabled))
        .collect()
}

/// Reference `_add_source_group`: an unavailable server without a tool
/// reports a failed discovery, and a plugin-owned source names its plugin.
#[test]
fn rows_report_a_failed_discovery_and_the_owning_plugin() {
    let sources = McpSource::published(&json!({"mcp": {"sources": [
        {"name": "broken", "kind": "server", "transport": "stdio",
         "status": "unavailable", "tools": []},
        {"name": "linear", "kind": "server", "transport": "http",
         "status": "connected", "pluginName": "linear-plugin",
         "tools": [{"name": "create_issue", "description": "", "enabled": true}]},
    ]}}))
    .expect("the sources decode");
    let descriptions = mcp_overlay(&sources)
        .items
        .into_iter()
        .filter(|item| !item.disabled)
        .map(|item| item.description)
        .collect::<Vec<_>>();
    assert_eq!(
        descriptions,
        [
            "http · plugin:linear-plugin · 1 tool · connected",
            "stdio · tool discovery failed · error - check your config",
        ]
    );
}

/// Reference `_show_detail_view`: a source that failed to bootstrap shows its
/// error before anything its status would show, and opens on it rather than on
/// the sign-in.
#[test]
fn a_bootstrap_error_replaces_the_detail_view() {
    let sources = McpSource::published(&json!({"mcp": {"sources": [
        {"name": "drive", "kind": "connector", "transport": "connector",
         "status": "needs_auth", "error": "catalog refused the token",
         "tools": [{"name": "search", "description": "", "enabled": false}]},
    ]}}))
    .expect("the sources decode");
    let row = mcp_overlay(&sources)
        .items
        .into_iter()
        .find(|item| !item.disabled)
        .expect("the connector row");
    assert!(matches!(
        row.action,
        OverlayAction::Integration(IntegrationTarget {
            requires_auth: false,
            ..
        })
    ));
    let target = IntegrationTarget {
        kind: IntegrationKind::Connector,
        ..server_target("drive")
    };
    assert_eq!(
        detail_rows(&mcp_detail_overlay(&sources, &target)),
        [
            ("Failed to bootstrap", true),
            ("catalog refused the token", true)
        ]
    );
}

/// Reference `_show_detail_view`: an unavailable server without a tool says
/// its discovery failed, with the error the state published for it.
#[test]
fn a_failed_discovery_shows_the_published_error() {
    let sources = McpSource::published(&json!({"mcp": {
        "sources": [
            {"name": "broken", "kind": "server", "transport": "stdio",
             "status": "unavailable", "tools": []},
            {"name": "quiet", "kind": "server", "transport": "stdio",
             "status": "unavailable", "tools": []},
            {"name": "idle", "kind": "server", "transport": "stdio",
             "status": "enabled", "tools": []},
        ],
        "discoveryErrors": {"broken": "spawn failed: no such file"},
    }}))
    .expect("the sources decode");
    assert_eq!(
        detail_rows(&mcp_detail_overlay(&sources, &server_target("broken"))),
        [
            ("Tool discovery failed", true),
            ("spawn failed: no such file", true)
        ]
    );
    assert_eq!(
        detail_rows(&mcp_detail_overlay(&sources, &server_target("quiet"))),
        [("Tool discovery failed", true)]
    );
    assert_eq!(
        detail_rows(&mcp_detail_overlay(&sources, &server_target("idle"))),
        [("No tools discovered", true)]
    );
}

/// Reference `MCPResource.read`: the connector catalog's rows, labeled by
/// their bootstrap display name, replace the connectors `mcp/read` published;
/// a failed read keeps them and an answer without a session drops them.
#[test]
fn the_connector_catalog_replaces_the_published_connectors() {
    let catalog = json!({"session": {"sources": [
        {"alias": "Google_Drive", "displayName": "Google Drive", "status": "connected",
         "tools": [{"name": "search", "description": null, "enabled": true}],
         "error": null},
    ]}});
    let rows = |sources: Vec<McpSource>| {
        sources
            .into_iter()
            .map(|source| (source.name, source.display_name, source.tools.len()))
            .collect::<Vec<_>>()
    };
    let merged = McpSource::with_catalog_connectors(published(), Some(&catalog))
        .expect("the catalog decodes");
    assert_eq!(
        rows(merged),
        [
            ("local".to_owned(), "local".to_owned(), 0),
            ("Google_Drive".to_owned(), "Google Drive".to_owned(), 1),
        ]
    );
    assert_eq!(
        McpSource::with_catalog_connectors(published(), None).expect("kept"),
        published()
    );
    let sessionless = McpSource::with_catalog_connectors(published(), Some(&json!({})))
        .expect("an answer without a session");
    assert_eq!(
        rows(sessionless),
        [("local".to_owned(), "local".to_owned(), 0)]
    );
}

/// Reference `_filter_sources`: the query fuzzy-matches the display name only,
/// each group orders by score, and a group with no match drops its heading
/// and the gap before it. The list lands on its best match.
#[test]
fn the_query_fuzzy_matches_the_display_name_best_score_first() {
    let sources = McpSource::published(&json!({"mcp": {"sources": [
        {"name": "inbox", "displayName": "Gmail", "kind": "connector",
         "transport": "connector", "status": "connected", "tools": []},
        {"name": "archive", "displayName": "Mail Archive", "kind": "connector",
         "transport": "connector", "status": "connected", "tools": []},
        {"name": "mailer", "displayName": "Local", "kind": "server",
         "transport": "stdio", "status": "connected", "tools": []},
    ]}}))
    .expect("the sources decode");
    let mut overlay = mcp_overlay(&sources);
    overlay.move_selection(2);
    overlay.set_query("mail");
    let visible = overlay
        .visible_items()
        .into_iter()
        .map(|(selected, item)| (selected, item.id.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(
        visible,
        [
            (false, "heading:connectors"),
            (true, "integration:connector:archive"),
            (false, "integration:connector:inbox"),
        ]
    );
}
