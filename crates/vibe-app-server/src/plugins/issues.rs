//! What resolving a session's plugins reported, as the catalogs and a reload
//! publish it.
//!
//! Reference `plugin_issues` and `plugin_reload_notices`
//! (`vibe/app_server/_plugins.py`).

use serde_json::{Value, json};
use vibe_core::plugins::PluginConfigIssue;

use super::SessionPlugins;

/// The reload notice codes: a source a rescan cannot repair. Reference
/// `_RELOAD_NOTICE_CODES`.
const RELOAD_NOTICE_CODES: [&str; 3] = [
    "plugin.mcp.connection_failed",
    "plugin.connector.unavailable",
    "plugin.connector.runtime_unavailable",
];

/// The resolution diagnostics as `ConfigIssue`s, sorted. Reference
/// `plugin_issues`.
#[must_use]
pub fn plugin_issues(plugins: &SessionPlugins) -> Vec<Value> {
    sorted_issues(plugins.issues())
}

/// [`plugin_issues`] over the installed resolve, which is what the runtime
/// snapshot lists.
#[must_use]
pub fn runtime_plugin_issues(plugins: &SessionPlugins) -> Vec<Value> {
    sorted_issues(&plugins.installed_issues)
}

fn sorted_issues(issues: &[PluginConfigIssue]) -> Vec<Value> {
    let mut issues: Vec<&PluginConfigIssue> = issues.iter().collect();
    issues.sort_by(|left, right| {
        (
            left.file.as_os_str(),
            left.code.as_deref().unwrap_or(""),
            &left.message,
        )
            .cmp(&(
                right.file.as_os_str(),
                right.code.as_deref().unwrap_or(""),
                &right.message,
            ))
    });
    issues
        .into_iter()
        .map(|issue| json!({"file": issue.file.to_string_lossy(), "message": issue.message}))
        .collect()
}

/// The remarks a successful reload still has to make. Reference
/// `plugin_reload_notices`.
#[must_use]
pub fn plugin_reload_notices(plugins: &SessionPlugins) -> Vec<String> {
    let mut issues: Vec<&PluginConfigIssue> = plugins.issues().iter().collect();
    issues.sort_by(|left, right| left.message.cmp(&right.message));
    let mut notices: Vec<String> = issues
        .into_iter()
        .filter(|issue| {
            issue
                .code
                .as_deref()
                .is_some_and(|code| RELOAD_NOTICE_CODES.contains(&code))
        })
        .map(|issue| issue.message.clone())
        .collect();
    let mut unavailable: Vec<String> = plugins
        .routes
        .iter()
        .filter(|(_, status)| **status == "unavailable")
        .map(|((group, function), _)| format!("{group}.{function}"))
        .collect();
    unavailable.sort();
    if !unavailable.is_empty() {
        notices.push(format!(
            "These plugin tools are no longer offered by their source and calling one will fail: {}.",
            unavailable.join(", ")
        ));
    }
    notices
}
