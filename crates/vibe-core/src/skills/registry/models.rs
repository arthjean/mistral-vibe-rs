//! The registry's wire vocabulary.
//!
//! Reference `vibe/core/skills/registry/models.py`: every payload model
//! ignores unknown fields and accepts both the camel-case wire spelling and
//! the snake-case attribute spelling, which `populate_by_name` grants upstream
//! and `#[serde(alias)]` grants here. The name sanitizer mirrors
//! `SkillMetadata.name`'s charset and length so a sanitized name always
//! validates.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use serde::Deserialize;

/// The length cap `sanitize_skill_name` truncates to, mirroring the skill
/// schema's `name` constraint so a sanitized name always validates.
const MAX_NAME_LEN: usize = 64;

/// Collapses a raw registry string into a valid skill name, or [`None`] when
/// nothing survives.
///
/// Reference `sanitize_skill_name`: trim, lowercase, collapse every run of
/// characters outside `[a-z0-9]` into one hyphen, strip hyphens at both ends,
/// cap at 64 characters and strip again, and answer `None` for an empty
/// result.
#[must_use]
pub fn sanitize_skill_name(raw: &str) -> Option<String> {
    let mut collapsed = String::new();
    let mut pending_hyphen = false;
    for character in raw.trim().chars().flat_map(char::to_lowercase) {
        if character.is_ascii_lowercase() || character.is_ascii_digit() {
            if pending_hyphen && !collapsed.is_empty() {
                collapsed.push('-');
            }
            pending_hyphen = false;
            collapsed.push(character);
        } else {
            pending_hyphen = true;
        }
    }
    collapsed.truncate(MAX_NAME_LEN);
    let trimmed = collapsed.trim_end_matches('-');
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

/// One asset of a registry skill payload, carrying its content as UTF-8 text
/// or base64 bytes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAssetContent {
    #[serde(default, alias = "text_content")]
    pub text_content: Option<String>,
    #[serde(default, alias = "raw_content")]
    pub raw_content: Option<String>,
    #[serde(default, alias = "is_executable")]
    pub is_executable: bool,
}

impl RegistryAssetContent {
    /// The asset's bytes: the UTF-8 text when present, the strictly decoded
    /// base64 otherwise, and [`None`] when the base64 is invalid or neither
    /// field is set, which the store answers by skipping the asset.
    #[must_use]
    pub fn to_bytes(&self) -> Option<Vec<u8>> {
        if let Some(text) = &self.text_content {
            return Some(text.clone().into_bytes());
        }
        if let Some(raw) = &self.raw_content {
            return BASE64.decode(raw).ok();
        }
        None
    }
}

/// The skill document a registry item carries: name, description, body and
/// assets keyed by their relative path.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySkillPayload {
    #[serde(default, alias = "skill_name")]
    pub skill_name: String,
    #[serde(default, alias = "skill_description")]
    pub skill_description: String,
    #[serde(default, alias = "skill_body")]
    pub skill_body: String,
    #[serde(default, alias = "skill_assets")]
    pub skill_assets: std::collections::BTreeMap<String, RegistryAssetContent>,
}

/// The item-level display attributes.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAttributes {
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: Option<String>,
}

/// The item-level registry metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryMetadata {
    #[serde(default)]
    pub name: String,
    #[serde(default, alias = "latest_version")]
    pub latest_version: i64,
    #[serde(default, alias = "sharing_scope")]
    pub sharing_scope: String,
    #[serde(default, alias = "created_at")]
    pub created_at: String,
    #[serde(default, alias = "last_modified_at")]
    pub last_modified_at: String,
    #[serde(default, alias = "created_by")]
    pub created_by: String,
}

/// The version-level registry metadata.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersionMetadata {
    #[serde(default, alias = "created_at")]
    pub created_at: String,
}

/// The version-level display attributes, carrying the author aliases.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersionAttributes {
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub aliases: Vec<String>,
}

/// One skill at one version, as the catalog and the single-skill endpoint
/// answer it.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySkillItem {
    #[serde(default, alias = "skill_id")]
    pub skill_id: String,
    #[serde(default)]
    pub skill: RegistrySkillPayload,
    #[serde(default)]
    pub attributes: RegistryAttributes,
    #[serde(default)]
    pub metadata: RegistryMetadata,
    #[serde(default)]
    pub version: i64,
    #[serde(default, alias = "version_metadata")]
    pub version_metadata: RegistryVersionMetadata,
    #[serde(default, alias = "version_attributes")]
    pub version_attributes: RegistryVersionAttributes,
}

impl RegistrySkillItem {
    /// The name the item publishes under: the metadata name, the payload name
    /// and the attribute title in that order, each through the sanitizer, then
    /// a `skill-<id>` fallback built from the full id with its hyphens
    /// removed, and [`None`] when the item carries no identifier at all, which
    /// callers answer by skipping the item rather than publishing a
    /// placeholder.
    #[must_use]
    pub fn resolved_name(&self) -> Option<String> {
        for candidate in [
            &self.metadata.name,
            &self.skill.skill_name,
            &self.attributes.title,
        ] {
            if let Some(name) = sanitize_skill_name(candidate) {
                return Some(name);
            }
        }
        if self.skill_id.is_empty() {
            return None;
        }
        // The id-based fallback runs through the same sanitizer so it obeys
        // the charset, length and hyphen rules the skill schema enforces, and
        // the full id rather than a prefix keeps unnamed skills
        // collision-free.
        sanitize_skill_name(&format!("skill-{}", self.skill_id.replace('-', "")))
    }

    /// The description the item publishes: the payload's when it carries
    /// non-whitespace text, the attribute description next, and empty
    /// otherwise, trimmed either way.
    #[must_use]
    pub fn resolved_description(&self) -> String {
        let payload = self.skill.skill_description.trim();
        if !payload.is_empty() {
            return payload.to_owned();
        }
        if let Some(description) = &self.attributes.description {
            let trimmed = description.trim();
            if !trimmed.is_empty() {
                return trimmed.to_owned();
            }
        }
        String::new()
    }
}

/// One catalog page.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListSkillsResponse {
    #[serde(default)]
    pub data: Vec<RegistrySkillItem>,
    #[serde(default, alias = "next_page_token")]
    pub next_page_token: String,
}

/// One row of the versions endpoint, projected: a concrete version and its
/// author aliases. The reserved `latest` alias is dynamic and never part of
/// this list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillVersionInfo {
    pub version: i64,
    pub aliases: Vec<String>,
}

/// One raw row of the versions endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistryVersionRow {
    pub version: i64,
    #[serde(default, alias = "version_attributes")]
    pub version_attributes: RegistryVersionAttributes,
}

impl RegistryVersionRow {
    #[must_use]
    pub fn to_info(&self) -> SkillVersionInfo {
        SkillVersionInfo {
            version: self.version,
            aliases: self.version_attributes.aliases.clone(),
        }
    }
}

/// The versions endpoint response.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListVersionsResponse {
    #[serde(default)]
    pub items: Vec<RegistryVersionRow>,
}
