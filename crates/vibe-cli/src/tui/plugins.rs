//! `/plugins` and `/reload-plugins`: the catalog a unified session's plugins
//! publish, browsed in a panel, and the re-pin that reports what it moved.
//!
//! Reference `PluginsApp` and `plugin_reload_report`
//! (`vibe/cli/textual_ui/widgets/plugins_app.py`), `PluginCatalogResource`
//! and `_plugin_changes` (`vibe/app_server/_integration_resources.py`), and
//! the `_show_plugins` and `_reload_plugins` handlers
//! (`vibe/cli/textual_ui/app.py`). The rows, titles and messages are the
//! reference's copy; the panel is drawn as every other list overlay here is.

use std::collections::{BTreeMap, BTreeSet};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::{Value, json};
use vibe_app_server::client::{ClientError, ProtocolErrorCode};
use vibe_core::events::PublicNoticeLevel;

use super::interaction::{Overlay, OverlayItem, OverlayKind};
use super::state::{EntrySource, EntryStatus, TranscriptEntry, TranscriptKind, TuiState};
use super::{InteractiveRuntime, push_local_document};

#[cfg(test)]
#[path = "plugins/plugins_tests.rs"]
mod plugins_tests;

pub(super) const NO_PLUGINS: &str = "This session resolves no plugins.";
pub(super) const NOTHING_INSTALLED: &str = "No plugins are installed for this session.";
pub(super) const OPENED: &str = "Plugins opened...";
pub(super) const CLOSED: &str = "Plugins closed.";

const LIST_HELP: &str = "↑↓/jk Navigate  Enter View  / Search  r Reload  Esc Close";
const DETAIL_HELP: &str = "Backspace Back  r Reload  Esc Close";
const FILTER_HELP: &str = "Enter Apply  Esc Clear";
const OPTION_PREFIX: &str = "plugin:";
const DIGEST_WIDTH: usize = 8;
const UNKNOWN: &str = "—";

/// One component a plugin contributes, as the catalog lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginComponent {
    pub kind: String,
    pub name: String,
    pub status: Option<String>,
}

/// One plugin the session runs. Reference `PluginCatalogEntry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginEntry {
    pub name: String,
    pub version: Option<String>,
    pub source_format: String,
    pub description: String,
    pub author: Option<String>,
    pub scope: Option<String>,
    pub content_sha256: Option<String>,
    pub installed_root: Option<String>,
    pub components: Vec<PluginComponent>,
    pub drifted: u64,
}

/// A plugin file the session could not load. Reference `PluginCatalogDropped`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DroppedPlugin {
    pub file: String,
    pub message: String,
}

/// Reference `PluginCatalogState`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct PluginCatalog {
    pub plugins: Vec<PluginEntry>,
    pub dropped: Vec<DroppedPlugin>,
}

impl PluginCatalog {
    /// Reads the `plugins` field `plugin_catalog/read` answers.
    pub(super) fn from_value(value: &Value) -> Self {
        let text = |value: &Value, key: &str| {
            value
                .get(key)
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        };
        let list = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        Self {
            plugins: list("plugins")
                .iter()
                .map(|entry| PluginEntry {
                    name: text(entry, "name").unwrap_or_default(),
                    version: text(entry, "version"),
                    source_format: text(entry, "sourceFormat").unwrap_or_default(),
                    description: text(entry, "description").unwrap_or_default(),
                    author: text(entry, "author"),
                    scope: text(entry, "scope"),
                    content_sha256: text(entry, "contentSha256"),
                    installed_root: text(entry, "installedRoot"),
                    components: entry
                        .get("components")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .map(|component| PluginComponent {
                            kind: text(component, "kind").unwrap_or_default(),
                            name: text(component, "name").unwrap_or_default(),
                            status: text(component, "status"),
                        })
                        .collect(),
                    drifted: entry
                        .get("drifted")
                        .and_then(Value::as_u64)
                        .unwrap_or_default(),
                })
                .collect(),
            dropped: list("dropped")
                .iter()
                .map(|dropped| DroppedPlugin {
                    file: text(dropped, "file").unwrap_or_default(),
                    message: text(dropped, "message").unwrap_or_default(),
                })
                .collect(),
        }
    }

    pub(super) fn is_empty(&self) -> bool {
        self.plugins.is_empty() && self.dropped.is_empty()
    }
}

/// How a plugin's pinned digest moved across a reload. Reference
/// `PluginCatalogChange`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginChange {
    pub name: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

/// Compares two catalogs on content digest, by plugin name in sorted order.
/// A plugin listed without a digest counts as absent, as a lookup that finds
/// nothing does in the reference.
pub(super) fn plugin_changes(before: &PluginCatalog, after: &PluginCatalog) -> Vec<PluginChange> {
    let digests = |catalog: &PluginCatalog| -> BTreeMap<String, Option<String>> {
        catalog
            .plugins
            .iter()
            .map(|entry| (entry.name.clone(), entry.content_sha256.clone()))
            .collect()
    };
    let pinned = digests(before);
    let current = digests(after);
    let names: BTreeSet<&String> = pinned.keys().chain(current.keys()).collect();
    names
        .into_iter()
        .filter_map(|name| {
            let before = pinned.get(name).cloned().flatten();
            let after = current.get(name).cloned().flatten();
            (before != after).then(|| PluginChange {
                name: name.clone(),
                before,
                after,
            })
        })
        .collect()
}

/// What a reload moved, by digest, rather than that it ran.
pub(super) fn reload_report(changes: &[PluginChange], after: &PluginCatalog) -> String {
    if changes.is_empty() {
        return "Plugins reloaded. Nothing changed.".to_owned();
    }
    let versions: BTreeMap<&str, Option<&str>> = after
        .plugins
        .iter()
        .map(|entry| (entry.name.as_str(), entry.version.as_deref()))
        .collect();
    let mut lines = vec!["### Plugins reloaded".to_owned(), String::new()];
    lines.extend(changes.iter().map(|change| {
        let version = versions.get(change.name.as_str()).copied().flatten();
        change_line(change, version)
    }));
    lines.join("\n")
}

fn change_line(change: &PluginChange, version: Option<&str>) -> String {
    match (&change.before, &change.after) {
        (None, _) => match version.filter(|version| !version.is_empty()) {
            Some(version) => format!("- `+` `{}` {version}", change.name),
            None => format!("- `+` `{}`", change.name),
        },
        (Some(_), None) => format!("- `-` `{}` — no longer installed", change.name),
        (Some(before), Some(after)) => format!(
            "- `~` `{}` {} → {}",
            change.name,
            short_digest(Some(before)),
            short_digest(Some(after))
        ),
    }
}

/// The first characters of a digest, or the unknown mark.
pub(super) fn short_digest(digest: Option<&str>) -> String {
    digest.map_or_else(
        || UNKNOWN.to_owned(),
        |digest| digest.chars().take(DIGEST_WIDTH).collect(),
    )
}

/// A plugin's list row: its name padded to `width`, then where it comes from.
pub(super) fn entry_label(entry: &PluginEntry, width: usize) -> String {
    let facts = [
        entry.scope.clone().unwrap_or_else(|| UNKNOWN.to_owned()),
        entry.source_format.clone(),
        short_digest(entry.content_sha256.as_deref()),
    ]
    .join(" · ");
    let mut label = format!("  {:<width$}  {facts}", entry.name);
    if entry.drifted > 0 {
        label.push_str(&format!(" · ⚠ {} drifted", entry.drifted));
    }
    if entry.installed_root.is_none() {
        label.push_str(" · uninstalled since pin");
    }
    label
}

/// The detail view of one plugin.
pub(super) fn detail_lines(entry: &PluginEntry, home: Option<&str>) -> Vec<String> {
    let known = |value: &Option<String>| value.clone().filter(|value| !value.is_empty());
    let mut lines = vec![
        format!(
            "  Author: {}",
            known(&entry.author).unwrap_or_else(|| UNKNOWN.to_owned())
        ),
        format!(
            "  Version: {}",
            known(&entry.version).unwrap_or_else(|| UNKNOWN.to_owned())
        ),
    ];
    if !entry.description.is_empty() {
        lines.push(String::new());
        lines.push(format!("  {}", entry.description));
    }
    lines.push(String::new());
    lines.push(match &entry.installed_root {
        None => "  Location: (uninstalled since pin)".to_owned(),
        Some(root) => format!("  Location: {}", abbreviate_home(root, home)),
    });
    lines.push(format!(
        "  Scope: {}",
        known(&entry.scope).unwrap_or_else(|| UNKNOWN.to_owned())
    ));
    lines.push(format!("  Format: {}", entry.source_format));
    lines.push(format!(
        "  Pinned: {}",
        short_digest(entry.content_sha256.as_deref())
    ));
    if entry.components.is_empty() {
        return lines;
    }
    lines.push(String::new());
    lines.push("  Components:".to_owned());
    lines.extend(component_lines(&entry.components));
    lines
}

/// The components grouped by kind, kinds in the order they first appear.
fn component_lines(components: &[PluginComponent]) -> Vec<String> {
    let mut grouped: Vec<(&str, Vec<String>)> = Vec::new();
    for component in components {
        let name = match &component.status {
            None => component.name.clone(),
            Some(status) => format!("{} ({status})", component.name),
        };
        match grouped.iter_mut().find(|(kind, _)| *kind == component.kind) {
            Some((_, names)) => names.push(name),
            None => grouped.push((component.kind.as_str(), vec![name])),
        }
    }
    grouped
        .into_iter()
        .map(|(kind, names)| format!("  ● {}: {}", component_label(kind), names.join(", ")))
        .collect()
}

fn component_label(kind: &str) -> &'static str {
    match kind {
        "skill" => "Skills",
        "knowledge" => "Knowledge",
        "library" => "Libraries",
        "mcp_server" => "MCP servers",
        "connector" => "Connectors",
        "hook" => "Hooks",
        "agent" => "Agents",
        "subagent" => "Subagents",
        "tool" => "Tools",
        _ => "Other",
    }
}

/// One file the session could not load, under the "Not loaded" heading.
pub(super) fn dropped_line(dropped: &DroppedPlugin, home: Option<&str>) -> String {
    format!(
        "  ! {} — {}",
        abbreviate_home(&dropped.file, home),
        dropped.message
    )
}

/// Writes a path under the home directory as `~`-relative, by plain prefix as
/// the reference does.
pub(super) fn abbreviate_home(path: &str, home: Option<&str>) -> String {
    match home {
        Some(home) if path.starts_with(home) => format!("~{}", &path[home.len()..]),
        _ => path.to_owned(),
    }
}

/// The open `/plugins` panel: the catalog it renders, the plugin it shows in
/// detail, and the filter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PluginsPanel {
    pub catalog: PluginCatalog,
    pub viewing: Option<String>,
    pub query: String,
    pub filtering: bool,
    pub home: Option<String>,
}

impl PluginsPanel {
    pub(super) fn new(catalog: PluginCatalog, home: Option<String>) -> Self {
        Self {
            catalog,
            viewing: None,
            query: String::new(),
            filtering: false,
            home,
        }
    }

    /// The overlay this state renders as. A reload can take the plugin being
    /// viewed away, which drops back to the list.
    pub(super) fn overlay(&mut self) -> Overlay {
        let viewing = self
            .viewing
            .as_ref()
            .and_then(|name| {
                self.catalog
                    .plugins
                    .iter()
                    .find(|entry| entry.name == *name)
            })
            .cloned();
        match viewing {
            Some(entry) => {
                let items = detail_lines(&entry, self.home.as_deref())
                    .into_iter()
                    .enumerate()
                    .map(|(index, line)| {
                        OverlayItem::new(format!("detail:{index}"), line, "", true)
                    })
                    .collect();
                let mut overlay = Overlay::new(OverlayKind::Plugins, entry.name, items);
                overlay.help = Some(DETAIL_HELP);
                overlay
            }
            None => {
                self.viewing = None;
                self.list_overlay()
            }
        }
    }

    fn list_overlay(&self) -> Overlay {
        let matching: Vec<&PluginEntry> = self
            .catalog
            .plugins
            .iter()
            .filter(|entry| self.matches(entry))
            .collect();
        let mut items = Vec::new();
        if matching.is_empty() {
            let placeholder = if self.query.is_empty() {
                "No plugins in this session"
            } else {
                "No plugins match this filter"
            };
            items.push(OverlayItem::new("placeholder", placeholder, "", true));
        } else {
            let width = matching
                .iter()
                .map(|entry| entry.name.chars().count())
                .max()
                .unwrap_or(0);
            items.extend(matching.iter().map(|entry| {
                OverlayItem::new(
                    format!("{OPTION_PREFIX}{}", entry.name),
                    entry_label(entry, width),
                    "",
                    false,
                )
            }));
        }
        if !self.catalog.dropped.is_empty() {
            items.push(OverlayItem::new("dropped:gap", "", "", true));
            items.push(OverlayItem::new("dropped:heading", "Not loaded", "", true));
            items.extend(
                self.catalog
                    .dropped
                    .iter()
                    .enumerate()
                    .map(|(index, dropped)| {
                        OverlayItem::new(
                            format!("dropped:{index}"),
                            dropped_line(dropped, self.home.as_deref()),
                            "",
                            true,
                        )
                    }),
            );
        }
        if self.filtering {
            let shown = if self.query.is_empty() {
                "Filter plugins".to_owned()
            } else {
                format!("/ {}", self.query)
            };
            items.push(OverlayItem::new("filter", shown, "", true));
        }
        let mut overlay = Overlay::new(
            OverlayKind::Plugins,
            format!("Plugins · {} in this session", self.catalog.plugins.len()),
            items,
        );
        overlay.help = Some(if self.filtering {
            FILTER_HELP
        } else {
            LIST_HELP
        });
        overlay
    }

    fn matches(&self, entry: &PluginEntry) -> bool {
        let query = self.query.trim().to_lowercase();
        query.is_empty()
            || format!("{} {}", entry.name, entry.description)
                .to_lowercase()
                .contains(&query)
    }
}

/// The two calls the catalog commands make.
pub(super) trait PluginsPort {
    /// The catalog, or `None` from a backend that resolves no plugins.
    fn read_catalog(&mut self) -> Result<Option<PluginCatalog>, String>;
    fn reload(&mut self) -> Result<(), String>;
}

/// What `/reload-plugins` came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ReloadOutcome {
    NoPlugins,
    Failed(String),
    Reloaded {
        report: String,
        after: PluginCatalog,
    },
}

/// Reads the catalog, re-pins, reads it again, and reports the difference.
pub(super) fn reload_plugins(port: &mut dyn PluginsPort) -> ReloadOutcome {
    let before = match port.read_catalog() {
        Ok(Some(before)) => before,
        Ok(None) => return ReloadOutcome::NoPlugins,
        Err(error) => return ReloadOutcome::Failed(error),
    };
    if let Err(error) = port.reload() {
        return ReloadOutcome::Failed(error);
    }
    match port.read_catalog() {
        Ok(Some(after)) => ReloadOutcome::Reloaded {
            report: reload_report(&plugin_changes(&before, &after), &after),
            after,
        },
        Ok(None) => ReloadOutcome::NoPlugins,
        Err(error) => ReloadOutcome::Failed(error),
    }
}

/// The error line a failed reload shows.
pub(super) fn reload_failure(error: &str) -> String {
    format!("Failed to reload plugins: {error}")
}

impl PluginsPort for InteractiveRuntime {
    fn read_catalog(&mut self) -> Result<Option<PluginCatalog>, String> {
        let params = json!({"sessionId": self.session_id});
        match self.service.public_call("plugin_catalog/read", params) {
            Ok(result) => Ok(Some(PluginCatalog::from_value(
                result.get("plugins").unwrap_or(&Value::Null),
            ))),
            Err(ClientError::Protocol(
                ProtocolErrorCode::NotImplemented | ProtocolErrorCode::MethodNotFound,
                _,
            )) => Ok(None),
            Err(error) => Err(client_message(error)),
        }
    }

    fn reload(&mut self) -> Result<(), String> {
        let params = json!({"sessionId": self.session_id});
        self.service
            .public_call("plugin/reload", params)
            .map(|_| ())
            .map_err(client_message)
    }
}

fn client_message(error: ClientError) -> String {
    match error {
        ClientError::Protocol(_, message) => message,
        error => error.to_string(),
    }
}

/// Routes one key to the open `/plugins` panel.
pub(in crate::tui) fn handle_plugins_key(
    key: KeyEvent,
    runtime: &mut Option<InteractiveRuntime>,
    state: &mut TuiState,
) {
    let Some(runtime) = runtime.as_mut() else {
        state.overlay = None;
        return;
    };
    let Some(mut panel) = runtime.plugins_panel.take() else {
        state.overlay = None;
        return;
    };
    let control = key.modifiers.contains(KeyModifiers::CONTROL);
    if panel.filtering {
        match key.code {
            KeyCode::Esc => {
                panel.query.clear();
                panel.filtering = false;
            }
            KeyCode::Enter => panel.filtering = false,
            KeyCode::Backspace => {
                panel.query.pop();
            }
            KeyCode::Char(character) if !control => panel.query.push(character),
            _ => {}
        }
    } else {
        let bare = key.modifiers.is_empty();
        match key.code {
            KeyCode::Esc => {
                state.overlay = None;
                push_local_document(state, CLOSED.to_owned());
                return;
            }
            KeyCode::Up | KeyCode::Char('k') if bare => {
                move_selection(state, -1);
                runtime.plugins_panel = Some(panel);
                return;
            }
            KeyCode::Down | KeyCode::Char('j') if bare => {
                move_selection(state, 1);
                runtime.plugins_panel = Some(panel);
                return;
            }
            KeyCode::Backspace if bare => panel.viewing = None,
            KeyCode::Char('/') if panel.viewing.is_none() => panel.filtering = true,
            KeyCode::Char('r') if bare => {
                if let ReloadOutcome::Reloaded { after, .. } = report_reload(runtime, state) {
                    panel.catalog = after;
                }
            }
            KeyCode::Enter if bare => {
                if let Some(name) = state
                    .overlay
                    .as_ref()
                    .and_then(Overlay::selected_item)
                    .and_then(|item| item.id.strip_prefix(OPTION_PREFIX))
                {
                    panel.viewing = Some(name.to_owned());
                }
            }
            _ => {}
        }
    }
    state.overlay = Some(panel.overlay());
    runtime.plugins_panel = Some(panel);
}

fn move_selection(state: &mut TuiState, delta: isize) {
    if let Some(overlay) = state.overlay.as_mut() {
        overlay.move_selection(delta);
    }
}

/// Runs a reload from the panel and writes its outcome into the transcript, as
/// `/reload-plugins` does.
fn report_reload(runtime: &mut InteractiveRuntime, state: &mut TuiState) -> ReloadOutcome {
    let outcome = reload_plugins(runtime);
    match &outcome {
        ReloadOutcome::NoPlugins => {
            push_local_document(state, NO_PLUGINS.to_owned());
        }
        ReloadOutcome::Failed(error) => {
            state.append_local(TranscriptEntry {
                id: String::new(),
                revision: 1,
                kind: TranscriptKind::Notice,
                text: reload_failure(error),
                status: EntryStatus::Failed,
                source: EntrySource::notice(PublicNoticeLevel::Error),
            });
        }
        ReloadOutcome::Reloaded { report, .. } => {
            push_local_document(state, report.clone());
        }
    }
    outcome
}
