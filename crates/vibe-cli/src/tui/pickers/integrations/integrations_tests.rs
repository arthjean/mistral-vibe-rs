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
                "stdio · no tools · error - check your config"
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
    };
    assert_eq!(
        mcp_detail_overlay(&published(), &target).title,
        "Connector: Alpha Docs"
    );
}
