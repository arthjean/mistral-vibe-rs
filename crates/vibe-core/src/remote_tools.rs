//! Shared naming and availability rules for externally provided tools.
//!
//! MCP servers and connectors have genuinely different lifecycles: one owns a
//! live peer with an epoch to invalidate in-flight calls, the other resolves a
//! stateless backend behind a cached catalog. What they do share is how a
//! remote tool becomes a registry entry: alias normalization, the reference
//! naming rule each source publishes under, and the rule turning a provider's
//! state into a [`ToolAvailability`]. Those rules live here so the two
//! lifecycles cannot drift on the part that is actually common.

use std::collections::BTreeSet;

use crate::tools::{ToolAvailability, ToolError, ToolRegistry, ToolSource};

/// Longest connector alias the reference keeps.
const MAX_CONNECTOR_ALIAS: usize = 256;

/// The connector alias rule: everything outside `[A-Za-z0-9_-]` becomes `_`,
/// the edges are trimmed of `_` and `-`, the result is capped, and an alias
/// reduced to nothing falls back to a fixed name.
///
/// Case and hyphens survive, which is what makes a connector published here
/// resolve to the same name the reference publishes.
#[must_use]
pub(crate) fn normalize_alias(value: &str) -> String {
    let normalized = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    let trimmed = normalized
        .trim_matches(|character| matches!(character, '_' | '-'))
        .chars()
        .take(MAX_CONNECTOR_ALIAS)
        .collect::<String>();
    if trimmed.is_empty() {
        "unnamed".to_owned()
    } else {
        trimmed
    }
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
///
/// The reference publishes an MCP tool as `{alias}_{tool}` and a connector tool
/// as `connector_{alias}_{tool}`, so a per-tool disable entry written for one
/// client applies to the other. The remote tool name is kept as the provider
/// gave it; only the MCP path still sanitizes, because a remote name may carry
/// characters [`crate::tools::ToolSpec::validate`] refuses and a rejected
/// registration is worse than a renamed tool.
#[must_use]
pub(crate) fn public_tool_name(source: ToolSource, alias: &str, tool: &str) -> String {
    match source {
        ToolSource::Mcp => format!("{}_{}", sanitize_mcp_name(alias), sanitize_mcp_name(tool)),
        ToolSource::Connector => format!("connector_{}_{}", normalize_alias(alias), tool),
        ToolSource::BuiltIn => tool.to_owned(),
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
        // The reference publishes `{alias}_{tool}` with no source prefix.
        assert_eq!(
            public_tool_name(ToolSource::Mcp, "serena", "find_symbol"),
            "serena_find_symbol"
        );
        // MCP aliases are user-authored keys: punctuation and case survive,
        // and a remote name carrying a refused character is sanitized.
        assert_eq!(
            public_tool_name(ToolSource::Mcp, "docs-v2.1", "search/files"),
            "docs-v2.1_search_files"
        );
        // Connector aliases keep case and hyphens, and the remote tool name is
        // published verbatim.
        assert_eq!(
            public_tool_name(ToolSource::Connector, "GitHub-Enterprise", "listRepos"),
            "connector_GitHub-Enterprise_listRepos"
        );
        assert_eq!(
            public_tool_name(ToolSource::Connector, "  ??  ", "read"),
            "connector_unnamed_read"
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
