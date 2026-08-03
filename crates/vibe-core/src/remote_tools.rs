//! Shared naming and availability rules for externally provided tools.
//!
//! MCP servers and connectors have genuinely different lifecycles: one owns a
//! live peer with an epoch to invalidate in-flight calls, the other resolves a
//! stateless backend behind a cached catalog. What they do share is how a
//! remote tool becomes a registry entry: the same alias normalisation, the same
//! `{source}_{alias}_{tool}` naming, and the same rule turning a provider's
//! state into a [`ToolAvailability`]. Those rules live here so the two
//! lifecycles cannot drift on the part that is actually common.

use std::collections::BTreeSet;

use crate::tools::{ToolAvailability, ToolError, ToolRegistry, ToolSource};

/// Reduces `value` to lowercase alphanumerics, the connector naming rule.
#[must_use]
pub(crate) fn normalize_alias(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    normalized.trim_matches('_').to_owned()
}

/// Keeps the characters an MCP alias is allowed to carry.
///
/// Unlike connectors, MCP aliases are user-authored configuration keys and have
/// always admitted `-`, `.`, and case. Normalizing them further would rename
/// every tool of an existing server and orphan its persisted preferences.
#[must_use]
pub(crate) fn sanitize_mcp_name(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The registry name a remote tool is published under.
#[must_use]
pub(crate) fn public_tool_name(source: ToolSource, alias: &str, tool: &str) -> String {
    match source {
        ToolSource::Mcp => format!(
            "mcp_{}_{}",
            sanitize_mcp_name(alias),
            sanitize_mcp_name(tool)
        ),
        ToolSource::Connector => format!(
            "connector_{}_{}",
            normalize_alias(alias),
            normalize_alias(tool)
        ),
        ToolSource::BuiltIn | ToolSource::Custom => tool.to_owned(),
    }
}

/// Why a provider's tools are not currently callable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderReach {
    /// The provider answers calls right now.
    Ready,
    /// The provider exists but cannot be reached, so its tools stay listed.
    Unreachable,
}

/// The availability one tool of a provider should carry.
///
/// Disabling is a user decision and outranks reachability: a disabled tool is
/// reported as disabled even when its provider is down.
#[must_use]
pub(crate) fn tool_availability(
    provider_enabled: bool,
    disabled_tools: &BTreeSet<String>,
    reach: ProviderReach,
    tool: &str,
) -> ToolAvailability {
    if !provider_enabled || disabled_tools.contains(tool) {
        ToolAvailability::Disabled
    } else if reach == ProviderReach::Ready {
        ToolAvailability::Available
    } else {
        ToolAvailability::Unavailable
    }
}

/// Applies one availability to every tool of a provider, atomically.
///
/// Returns `false` when the registry no longer holds every named tool from this
/// source, which means the catalog changed underneath the caller.
pub(crate) fn set_all(
    tools: &ToolRegistry,
    source: ToolSource,
    names: &[String],
    availability: ToolAvailability,
) -> Result<bool, ToolError> {
    let updates = names
        .iter()
        .cloned()
        .map(|name| (name, availability))
        .collect::<Vec<_>>();
    tools.set_availabilities(source, &updates)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_names_keep_each_source_naming_rule() {
        // MCP aliases are user-authored keys: punctuation and case survive.
        assert_eq!(
            public_tool_name(ToolSource::Mcp, "docs-v2.1", "search/files"),
            "mcp_docs-v2.1_search_files"
        );
        assert_eq!(
            public_tool_name(ToolSource::Connector, "GitHub", "listRepos"),
            "connector_github_listrepos"
        );
    }

    #[test]
    fn disabling_outranks_unreachability() {
        let disabled = BTreeSet::from(["mcp_docs_read".to_owned()]);
        assert_eq!(
            tool_availability(true, &disabled, ProviderReach::Ready, "mcp_docs_read"),
            ToolAvailability::Disabled
        );
        assert_eq!(
            tool_availability(true, &disabled, ProviderReach::Unreachable, "mcp_docs_list"),
            ToolAvailability::Unavailable
        );
        assert_eq!(
            tool_availability(
                false,
                &BTreeSet::new(),
                ProviderReach::Ready,
                "mcp_docs_list"
            ),
            ToolAvailability::Disabled
        );
        assert_eq!(
            tool_availability(
                true,
                &BTreeSet::new(),
                ProviderReach::Ready,
                "mcp_docs_list"
            ),
            ToolAvailability::Available
        );
    }
}
