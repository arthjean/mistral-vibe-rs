//! Identifier-safe names for the tool groups a plugin's sources produce.
//!
//! Reference `vibe/core/plugins/_naming.py`. A group name is digested into the
//! snapshot, so it has to come out the same on every host: names are assigned
//! from the whole set of identities at once, ordered by their raw identity, and
//! a contested base name takes a suffix from a digest of its own identity,
//! growing from eight hex characters until it is free.

use std::collections::{BTreeMap, BTreeSet};

use sha2::{Digest, Sha256};

use super::compatibility::typescript_identifier;

const DIGEST_PREFIX: usize = 8;

/// Every character that is not an ASCII letter, digit, `_` or `$` becomes
/// `_`, and an empty value becomes `_`. Reference `identifier_segment`.
#[must_use]
pub fn identifier_segment(value: &str) -> String {
    let normalized: String = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' || character == '$' {
                character
            } else {
                '_'
            }
        })
        .collect();
    if normalized.is_empty() {
        "_".to_owned()
    } else {
        normalized
    }
}

/// The base group name of one plugin-owned MCP source. Reference
/// `plugin_mcp_group_name`.
#[must_use]
pub fn plugin_mcp_group_name(plugin_namespace: &str, source_id: &str) -> String {
    format!(
        "plugin_{}_{}",
        identifier_segment(plugin_namespace),
        identifier_segment(source_id)
    )
}

/// The identity a group name is assigned to. Reference `ToolGroupIdentity`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ToolGroupIdentity {
    pub plugin_name: String,
    pub base_name: String,
    pub source_id: String,
}

impl ToolGroupIdentity {
    fn key(&self) -> String {
        format!("{}\0{}", self.plugin_name, self.source_id)
    }

    fn digest(&self) -> String {
        let identity = [
            "plugin_mcp",
            &self.plugin_name,
            &self.base_name,
            &self.source_id,
        ]
        .join("\0");
        hex::encode(Sha256::digest(identity.as_bytes()))
    }
}

/// Assigns every identity a unique name, none of them in `claimed`.
/// Reference `resolve_tool_group_names`.
#[must_use]
pub fn resolve_tool_group_names<'a>(
    identities: impl IntoIterator<Item = &'a ToolGroupIdentity>,
    claimed: impl IntoIterator<Item = String>,
) -> BTreeMap<ToolGroupIdentity, String> {
    let mut taken: BTreeSet<String> = claimed.into_iter().collect();
    let mut unique: Vec<&ToolGroupIdentity> = identities.into_iter().collect();
    unique.sort_by_key(|identity| identity.key());
    unique.dedup();
    let mut by_base: BTreeMap<&str, Vec<&ToolGroupIdentity>> = BTreeMap::new();
    for identity in unique {
        by_base
            .entry(identity.base_name.as_str())
            .or_default()
            .push(identity);
    }
    let mut resolved = BTreeMap::new();
    for (base_name, contenders) in by_base {
        if let [only] = contenders.as_slice()
            && !taken.contains(base_name)
        {
            resolved.insert((*only).clone(), base_name.to_owned());
            taken.insert(base_name.to_owned());
            continue;
        }
        for identity in contenders {
            let name = with_digest_suffix(identity, &taken);
            taken.insert(name.clone());
            resolved.insert(identity.clone(), name);
        }
    }
    resolved
}

fn with_digest_suffix(identity: &ToolGroupIdentity, taken: &BTreeSet<String>) -> String {
    let digest = identity.digest();
    (DIGEST_PREFIX..=digest.len())
        .map(|length| format!("{}_{}", identity.base_name, &digest[..length]))
        .find(|name| !taken.contains(name))
        // Sixty-four hex characters of SHA-256 cannot all be taken in practice;
        // the full digest is the last word either way.
        .unwrap_or_else(|| format!("{}_{digest}", identity.base_name))
}

/// The function name a tool is published under. Reference
/// `tool_function_name`.
#[must_use]
pub fn tool_function_name(source_tool_name: &str, override_name: Option<&str>) -> String {
    typescript_identifier(override_name.unwrap_or(source_tool_name))
}
