//! The plugin catalog's rows, detail, diff and reload report, against the
//! reference's copy.

use serde_json::json;

use super::*;

fn entry(name: &str, digest: Option<&str>) -> PluginEntry {
    PluginEntry {
        name: name.to_owned(),
        version: Some("0.1.0".to_owned()),
        source_format: "agent_plugins_1_0".to_owned(),
        description: format!("Capabilities provided by {name}."),
        author: None,
        scope: Some("global".to_owned()),
        content_sha256: digest.map(ToOwned::to_owned),
        installed_root: Some(format!("/home/user/.vibe/plugins/{name}")),
        components: Vec::new(),
        drifted: 0,
    }
}

fn catalog(entries: Vec<PluginEntry>) -> PluginCatalog {
    PluginCatalog {
        plugins: entries,
        dropped: Vec::new(),
    }
}

#[test]
fn the_catalog_reads_the_wire_shape_plugin_catalog_read_answers() {
    let parsed = PluginCatalog::from_value(&json!({
        "plugins": [{
            "name": "vibe",
            "version": "1.0.0",
            "sourceFormat": "agent_plugins_1_0",
            "manifestDigest": "70952cc1",
            "description": "Skills and components shipped with the Vibe CLI.",
            "author": "Mistral AI",
            "scope": "builtin",
            "contentSha256": "c32af761ef9b6f9a",
            "pinnedRoot": "/pins/c3",
            "installedRoot": null,
            "components": [{"kind": "skill", "name": "vibe:vibe", "status": null}],
            "drifted": 2,
        }],
        "dropped": [{"file": "/x/mcp.json", "message": "broken"}],
    }));
    let vibe = &parsed.plugins[0];
    assert_eq!(vibe.author.as_deref(), Some("Mistral AI"));
    assert_eq!(vibe.installed_root, None);
    assert_eq!(vibe.drifted, 2);
    assert_eq!(vibe.components[0].kind, "skill");
    assert_eq!(parsed.dropped[0].message, "broken");
    assert!(!parsed.is_empty());
    assert!(PluginCatalog::from_value(&json!({"plugins": [], "dropped": []})).is_empty());
}

#[test]
fn a_list_row_pads_the_name_and_flags_drift_and_a_vanished_checkout() {
    let mut demo = entry("demo", Some("a82a1ea9ab287e5f"));
    assert_eq!(
        entry_label(&demo, 6),
        "  demo    global · agent_plugins_1_0 · a82a1ea9"
    );
    demo.drifted = 3;
    demo.installed_root = None;
    demo.scope = None;
    demo.content_sha256 = None;
    assert_eq!(
        entry_label(&demo, 4),
        "  demo  — · agent_plugins_1_0 · — · ⚠ 3 drifted · uninstalled since pin"
    );
}

#[test]
fn the_detail_view_groups_components_by_kind_in_first_seen_order() {
    let mut demo = entry("demo", Some("a82a1ea9ab287e5f"));
    demo.components = vec![
        PluginComponent {
            kind: "mcp_server".to_owned(),
            name: "viash".to_owned(),
            status: None,
        },
        PluginComponent {
            kind: "tool".to_owned(),
            name: "plugin_demo_viash.shout".to_owned(),
            status: Some("unavailable".to_owned()),
        },
        PluginComponent {
            kind: "mcp_server".to_owned(),
            name: "other".to_owned(),
            status: None,
        },
    ];
    assert_eq!(
        detail_lines(&demo, Some("/home/user")),
        vec![
            "  Author: —",
            "  Version: 0.1.0",
            "",
            "  Capabilities provided by demo.",
            "",
            "  Location: ~/.vibe/plugins/demo",
            "  Scope: global",
            "  Format: agent_plugins_1_0",
            "  Pinned: a82a1ea9",
            "",
            "  Components:",
            "  ● MCP servers: viash, other",
            "  ● Tools: plugin_demo_viash.shout (unavailable)",
        ]
    );
    demo.installed_root = None;
    demo.components.clear();
    let lines = detail_lines(&demo, None);
    assert_eq!(lines[5], "  Location: (uninstalled since pin)");
    assert_eq!(lines.last().map(String::as_str), Some("  Pinned: a82a1ea9"));
}

#[test]
fn a_dropped_file_is_listed_home_relative() {
    let dropped = DroppedPlugin {
        file: "/home/user/.vibe/plugins/demo/mcp.json".to_owned(),
        message: "MCP server 'broken' could not be connected".to_owned(),
    };
    assert_eq!(
        dropped_line(&dropped, Some("/home/user")),
        "  ! ~/.vibe/plugins/demo/mcp.json — MCP server 'broken' could not be connected"
    );
    assert_eq!(abbreviate_home("/srv/x", Some("/home/user")), "/srv/x");
}

#[test]
fn the_diff_compares_digests_by_sorted_name() {
    let before = catalog(vec![
        entry("kept", Some("1111111111")),
        entry("moved", Some("2222222222")),
        entry("gone", Some("3333333333")),
        entry("undigested", None),
    ]);
    let after = catalog(vec![
        entry("new", Some("4444444444")),
        entry("kept", Some("1111111111")),
        entry("moved", Some("5555555555")),
    ]);
    let changes = plugin_changes(&before, &after);
    assert_eq!(
        changes
            .iter()
            .map(|change| change.name.as_str())
            .collect::<Vec<_>>(),
        vec!["gone", "moved", "new"]
    );
    assert_eq!(
        reload_report(&changes, &after),
        "### Plugins reloaded\n\n\
         - `-` `gone` — no longer installed\n\
         - `~` `moved` 22222222 → 55555555\n\
         - `+` `new` 0.1.0"
    );
    assert_eq!(
        reload_report(&plugin_changes(&after, &after), &after),
        "Plugins reloaded. Nothing changed."
    );
    let mut unversioned = after.clone();
    unversioned.plugins[0].version = None;
    assert_eq!(
        reload_report(&changes[2..], &unversioned),
        "### Plugins reloaded\n\n- `+` `new`"
    );
}

struct Port {
    reads: Vec<Result<Option<PluginCatalog>, String>>,
    reload: Result<(), String>,
}

impl PluginsPort for Port {
    fn read_catalog(&mut self) -> Result<Option<PluginCatalog>, String> {
        self.reads.remove(0)
    }

    fn reload(&mut self) -> Result<(), String> {
        self.reload.clone()
    }
}

#[test]
fn a_reload_reads_either_side_of_the_re_pin() {
    let before = catalog(vec![entry("demo", Some("1111111111"))]);
    let after = catalog(vec![entry("demo", Some("2222222222"))]);
    let outcome = reload_plugins(&mut Port {
        reads: vec![Ok(Some(before.clone())), Ok(Some(after.clone()))],
        reload: Ok(()),
    });
    assert_eq!(
        outcome,
        ReloadOutcome::Reloaded {
            report: "### Plugins reloaded\n\n- `~` `demo` 11111111 → 22222222".to_owned(),
            after,
        }
    );
    assert_eq!(
        reload_plugins(&mut Port {
            reads: vec![Ok(None)],
            reload: Ok(()),
        }),
        ReloadOutcome::NoPlugins
    );
    assert_eq!(
        reload_plugins(&mut Port {
            reads: vec![Ok(Some(before))],
            reload: Err("no writer".to_owned()),
        }),
        ReloadOutcome::Failed("no writer".to_owned())
    );
    assert_eq!(
        reload_failure("no writer"),
        "Failed to reload plugins: no writer"
    );
}

#[test]
fn the_panel_lists_filters_and_shows_detail() {
    let mut state = catalog(vec![
        entry("alpha", Some("1111111111")),
        entry("beta", None),
    ]);
    state.dropped.push(DroppedPlugin {
        file: "/x/mcp.json".to_owned(),
        message: "broken".to_owned(),
    });
    let mut panel = PluginsPanel::new(state, None);
    let overlay = panel.overlay();
    assert_eq!(overlay.title, "Plugins · 2 in this session");
    assert_eq!(overlay.help, Some(LIST_HELP));
    let labels: Vec<&str> = overlay
        .items
        .iter()
        .map(|item| item.label.as_str())
        .collect();
    assert_eq!(
        labels,
        vec![
            "  alpha  global · agent_plugins_1_0 · 11111111",
            "  beta   global · agent_plugins_1_0 · —",
            "",
            "Not loaded",
            "  ! /x/mcp.json — broken",
        ]
    );
    assert_eq!(
        overlay.selected_item().map(|item| item.id.as_str()),
        Some("plugin:alpha")
    );

    panel.filtering = true;
    panel.query = "BET".to_owned();
    let overlay = panel.overlay();
    assert_eq!(overlay.help, Some(FILTER_HELP));
    assert_eq!(
        overlay.items[0].label,
        "  beta  global · agent_plugins_1_0 · —"
    );
    panel.query = "zzz".to_owned();
    assert_eq!(
        panel.overlay().items[0].label,
        "No plugins match this filter"
    );

    panel.filtering = false;
    panel.query.clear();
    panel.viewing = Some("alpha".to_owned());
    let overlay = panel.overlay();
    assert_eq!(overlay.title, "alpha");
    assert_eq!(overlay.help, Some(DETAIL_HELP));
    assert!(overlay.selected_item().is_none());

    // A reload that took the viewed plugin away falls back to the list.
    panel.catalog.plugins.remove(0);
    assert_eq!(panel.overlay().title, "Plugins · 1 in this session");
    assert_eq!(panel.viewing, None);
}

#[test]
fn an_empty_panel_says_so() {
    let mut panel = PluginsPanel::new(PluginCatalog::default(), None);
    assert_eq!(panel.overlay().items[0].label, "No plugins in this session");
}

fn press(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// The panel's keys, driven against a legacy session, whose server declines
/// the catalog: a reload from the panel says so and leaves the panel as it was.
#[test]
fn the_panel_keys_navigate_view_filter_reload_and_close() {
    let mut runtime = Some(crate::tui::runtime::interactive_test_runtime(
        "plugins-panel-keys",
    ));
    let mut state = TuiState::new("plugins-panel-keys");
    let mut panel = PluginsPanel::new(
        catalog(vec![
            entry("alpha", Some("1111111111")),
            entry("beta", None),
        ]),
        None,
    );
    state.overlay = Some(panel.overlay());
    runtime.as_mut().unwrap().plugins_panel = Some(panel);

    handle_plugins_key(press(KeyCode::Char('j')), &mut runtime, &mut state);
    handle_plugins_key(press(KeyCode::Enter), &mut runtime, &mut state);
    assert_eq!(state.overlay.as_ref().unwrap().title, "beta");
    // `/` searches the list only.
    handle_plugins_key(press(KeyCode::Char('/')), &mut runtime, &mut state);
    assert_eq!(state.overlay.as_ref().unwrap().title, "beta");
    handle_plugins_key(press(KeyCode::Backspace), &mut runtime, &mut state);
    assert_eq!(
        state.overlay.as_ref().unwrap().title,
        "Plugins · 2 in this session"
    );

    handle_plugins_key(press(KeyCode::Char('/')), &mut runtime, &mut state);
    for character in "alp".chars() {
        handle_plugins_key(press(KeyCode::Char(character)), &mut runtime, &mut state);
    }
    handle_plugins_key(press(KeyCode::Enter), &mut runtime, &mut state);
    let overlay = state.overlay.as_ref().unwrap();
    assert_eq!(overlay.help, Some(LIST_HELP));
    assert_eq!(overlay.items.len(), 1);
    // `Esc` while filtering clears the filter rather than closing.
    handle_plugins_key(press(KeyCode::Char('/')), &mut runtime, &mut state);
    handle_plugins_key(press(KeyCode::Esc), &mut runtime, &mut state);
    assert_eq!(state.overlay.as_ref().unwrap().items.len(), 2);

    handle_plugins_key(press(KeyCode::Char('r')), &mut runtime, &mut state);
    assert_eq!(state.entries.last().unwrap().text, NO_PLUGINS);
    assert_eq!(state.overlay.as_ref().unwrap().items.len(), 2);

    handle_plugins_key(press(KeyCode::Esc), &mut runtime, &mut state);
    assert!(state.overlay.is_none());
    assert!(runtime.as_ref().unwrap().plugins_panel.is_none());
    assert_eq!(state.entries.last().unwrap().text, CLOSED);
}
