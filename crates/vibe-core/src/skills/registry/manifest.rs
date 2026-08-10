//! The installed-skill manifests, one global and one per project root.
//!
//! Reference `vibe/core/skills/registry/_manifest.py`: a manifest is a TOML
//! document listing installed registry skills, each pinned to a concrete
//! version or to an alias such as `latest`. The global manifest lives at
//! `{vibe_home}/skills.toml`; a project manifest lives at
//! `.vibe/skills.toml` under its root, and a root whose manifest resolves to
//! the global one is dropped, so project scope is never an alias for global.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The registry's reserved alias that always resolves to the newest version,
/// server-side. A pin set to this alias auto-resolves to latest and never
/// reports an update as available.
pub const REGISTRY_LATEST_ALIAS: &str = "latest";

/// A manifest entry's pin: a concrete version number frozen on disk, or an
/// alias string resolved server-side.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ManifestVersion {
    Frozen(i64),
    Alias(String),
}

impl Default for ManifestVersion {
    fn default() -> Self {
        Self::Alias(REGISTRY_LATEST_ALIAS.to_owned())
    }
}

/// One installed skill: its published name, its registry id, its pin and its
/// description.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ManifestEntry {
    pub name: String,
    pub skill_id: String,
    #[serde(default)]
    pub version: ManifestVersion,
    #[serde(default)]
    pub description: String,
}

impl ManifestEntry {
    /// The pinned alias, or [`None`] when the pin is frozen to a version
    /// number.
    #[must_use]
    pub fn alias(&self) -> Option<&str> {
        match &self.version {
            ManifestVersion::Alias(alias) => Some(alias),
            ManifestVersion::Frozen(_) => None,
        }
    }
}

/// One manifest document.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct SkillManifest {
    #[serde(default)]
    pub skills: Vec<ManifestEntry>,
}

impl SkillManifest {
    /// Replaces the entry of the same name, so the newest write wins and a
    /// name appears once.
    pub fn upsert(&mut self, entry: ManifestEntry) {
        self.skills.retain(|existing| existing.name != entry.name);
        self.skills.push(entry);
    }

    /// Removes the named entry, answering whether one was there to remove.
    pub fn remove(&mut self, name: &str) -> bool {
        let before = self.skills.len();
        self.skills.retain(|existing| existing.name != name);
        self.skills.len() != before
    }
}

/// A manifest read from disk, with the warning a malformed document raised.
///
/// The reference logs the warning and proceeds; this crate has no logger, so
/// the message travels with the result and the caller decides where it goes.
/// Either way a corrupt file never blocks startup.
#[derive(Debug, Clone, Default)]
pub struct LoadedManifest {
    pub manifest: SkillManifest,
    pub warning: Option<String>,
}

/// The global manifest location under a Vibe home.
#[must_use]
pub fn global_manifest_path(vibe_home: &Path) -> PathBuf {
    vibe_home.join("skills.toml")
}

/// The project-scoped manifest paths for the open project roots, resolved,
/// deduplicated, and never the global manifest: a root whose manifest
/// resolves to it (running from the home directory, where
/// `~/.vibe/skills.toml` *is* the global one) is dropped.
#[must_use]
pub fn project_manifest_paths(vibe_home: &Path, project_roots: &[PathBuf]) -> Vec<PathBuf> {
    let global = resolve_lenient(&global_manifest_path(vibe_home));
    let mut out: Vec<PathBuf> = Vec::new();
    for root in project_roots {
        let path = resolve_lenient(&root.join(".vibe").join("skills.toml"));
        if path == global || out.contains(&path) {
            continue;
        }
        out.push(path);
    }
    out
}

/// Reads one manifest. A missing or syntactically broken file answers an
/// empty manifest silently; a well-formed document of the wrong shape answers
/// an empty manifest with a warning naming the path and the reason.
#[must_use]
pub fn load(path: &Path) -> LoadedManifest {
    let Ok(text) = fs::read_to_string(path) else {
        return LoadedManifest::default();
    };
    let Ok(table) = text.parse::<toml::Table>() else {
        return LoadedManifest::default();
    };
    match SkillManifest::deserialize(toml::Value::Table(table)) {
        Ok(manifest) => LoadedManifest {
            manifest,
            warning: None,
        },
        Err(error) => LoadedManifest {
            manifest: SkillManifest::default(),
            warning: Some(format!(
                "ignoring the malformed skills manifest at {}: {error}",
                path.display()
            )),
        },
    }
}

/// Writes one manifest, creating the parent directories.
pub fn save(path: &Path, manifest: &SkillManifest) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, render(manifest))
}

/// The saved document, in the presentation the reference's writer produces:
/// one inline table per entry, keys in declaration order, four-space
/// indentation.
fn render(manifest: &SkillManifest) -> String {
    if manifest.skills.is_empty() {
        return "skills = []\n".to_owned();
    }
    let mut out = String::from("skills = [\n");
    for entry in &manifest.skills {
        let version = match &entry.version {
            ManifestVersion::Frozen(version) => version.to_string(),
            ManifestVersion::Alias(alias) => toml_string(alias),
        };
        out.push_str(&format!(
            "    {{ name = {}, skill_id = {}, version = {version}, description = {} }},\n",
            toml_string(&entry.name),
            toml_string(&entry.skill_id),
            toml_string(&entry.description),
        ));
    }
    out.push_str("]\n");
    out
}

/// One TOML basic string.
fn toml_string(value: &str) -> String {
    let mut quoted = String::from('"');
    for character in value.chars() {
        match character {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            '\u{8}' => quoted.push_str("\\b"),
            '\t' => quoted.push_str("\\t"),
            '\n' => quoted.push_str("\\n"),
            '\u{c}' => quoted.push_str("\\f"),
            '\r' => quoted.push_str("\\r"),
            other if other.is_control() => {
                quoted.push_str(&format!("\\u{:04X}", other as u32));
            }
            other => quoted.push(other),
        }
    }
    quoted.push('"');
    quoted
}

/// The reference's non-strict `resolve` over a path that may not exist yet:
/// the deepest existing ancestor is canonicalized and the remainder appended,
/// so two spellings of one location compare equal.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = fs::canonicalize(path) {
        return resolved;
    }
    let mut remainder = Vec::new();
    let mut cursor = path;
    while let Some(parent) = cursor.parent() {
        if let Some(name) = cursor.file_name() {
            remainder.push(name.to_owned());
        }
        if let Ok(resolved) = fs::canonicalize(parent) {
            let mut out = resolved;
            for part in remainder.iter().rev() {
                out.push(part);
            }
            return out;
        }
        cursor = parent;
    }
    path.to_path_buf()
}
