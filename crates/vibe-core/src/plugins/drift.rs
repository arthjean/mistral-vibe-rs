//! Measuring reconnected tool sources against a pinned catalog.
//!
//! Reference `vibe/core/plugins/_drift.py`. A route is `live` when its source
//! answered with the fingerprint the pin recorded, `stale` when the source
//! answered differently or without the tool, and `unavailable` when the source
//! did not answer at all.

use std::collections::{BTreeMap, BTreeSet};

use super::materialize::MaterializedPluginSet;
use super::snapshot::{
    PluginToolGroupSnapshot, PluginToolRouteSnapshot, ResolvedPluginSnapshot, build_plugin_snapshot,
};

/// `live`, `stale` or `unavailable`.
pub type PluginRouteStatus = &'static str;
/// `(group_name, function_name)`.
pub type PluginRouteKey = (String, String);

/// Statuses every pinned route. Reference `reconcile_plugin_routes`.
#[must_use]
pub fn reconcile_plugin_routes(
    pinned: &ResolvedPluginSnapshot,
    materialized: &MaterializedPluginSet,
) -> BTreeMap<PluginRouteKey, PluginRouteStatus> {
    let mut answered: BTreeSet<(String, &str, String)> = BTreeSet::new();
    for (plugin, source) in &materialized.connected_mcp_sources {
        answered.insert((plugin.clone(), "mcp", source.clone()));
    }
    for (plugin, source) in &materialized.connected_connector_sources {
        answered.insert((plugin.clone(), "connector", source.clone()));
    }
    pinned
        .tool_routes
        .iter()
        .map(|route| {
            let source = (
                route.plugin_name.clone(),
                route.source_kind.as_str(),
                route.source_id.clone(),
            );
            let status = if !answered.contains(&source) {
                "unavailable"
            } else if materialized
                .tool_routes
                .get(&route.key())
                .is_some_and(|derived| derived.schema_fingerprint == route.schema_fingerprint)
            {
                "live"
            } else {
                "stale"
            };
            (route.key(), status)
        })
        .collect()
}

/// Carries the tools a re-pin no longer derives into its catalog. Reference
/// `retain_pinned_tools`.
#[must_use]
pub fn retain_pinned_tools(
    current: &ResolvedPluginSnapshot,
    previous: &ResolvedPluginSnapshot,
) -> ResolvedPluginSnapshot {
    let derived: BTreeSet<PluginRouteKey> = current
        .tool_routes
        .iter()
        .map(PluginToolRouteSnapshot::key)
        .collect();
    let lost: Vec<&PluginToolRouteSnapshot> = previous
        .tool_routes
        .iter()
        .filter(|route| !derived.contains(&route.key()))
        .collect();
    if lost.is_empty() {
        return current.clone();
    }
    let lost_keys: BTreeSet<PluginRouteKey> = lost.iter().map(|route| route.key()).collect();
    let mut groups: Vec<PluginToolGroupSnapshot> = current.tool_groups.clone();
    for pinned_group in &previous.tool_groups {
        let retained: Vec<_> = pinned_group
            .tools
            .iter()
            .filter(|tool| lost_keys.contains(&(pinned_group.name.clone(), tool.name.clone())))
            .cloned()
            .collect();
        if retained.is_empty() {
            continue;
        }
        if let Some(group) = groups
            .iter_mut()
            .find(|group| group.name == pinned_group.name)
        {
            group.tools.extend(retained);
        } else {
            let mut group = pinned_group.clone();
            group.tools = retained;
            groups.push(group);
        }
    }
    let named: BTreeSet<&str> = current
        .plugins
        .iter()
        .map(|entry| entry.name.as_str())
        .collect();
    let unnamed: BTreeSet<&str> = lost
        .iter()
        .map(|route| route.plugin_name.as_str())
        .filter(|name| !named.contains(name))
        .collect();
    let mut snapshot = current.clone();
    snapshot.plugins.extend(
        previous
            .plugins
            .iter()
            .filter(|entry| unnamed.contains(entry.name.as_str()))
            .cloned(),
    );
    snapshot.tool_groups = groups;
    snapshot.tool_routes.extend(lost.into_iter().cloned());
    build_plugin_snapshot(snapshot.clone()).unwrap_or(snapshot)
}
