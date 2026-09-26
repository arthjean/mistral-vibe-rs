//! The write side of the registry service: a skill's details and versions,
//! and the pins a skills browser imports, moves, removes and converts.
//!
//! Reference `vibe/core/skills/registry/_service.py` (`get_skill_details`,
//! `get_skill_body`, `list_skill_versions`, `import_skill`,
//! `set_skill_version`, `set_skill_latest`, `set_skill_alias`,
//! `remove_skill`, `convert_skill_to_local`) and `_resolved.py`. A pin is
//! written to the global manifest or to the first project manifest of the
//! session's roots, never outside them.

use std::path::{Path, PathBuf};

use super::client::RegistrySkillsError;
use super::manifest::{self, ManifestEntry, ManifestVersion, REGISTRY_LATEST_ALIAS, SkillManifest};
use super::models::SkillVersionInfo;
use super::service::{RegistryEndpoint, open_client};
use super::store;

/// The `cache.toml` section recording what each custom alias resolved to.
const RESOLVED_SECTION: &str = "registry_resolved_aliases";

/// Which manifest a pin lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Global,
    Project,
}

/// Where the registry's files live under one Vibe home, and the project
/// roots a project-scoped pin may write into.
#[derive(Debug, Clone)]
pub struct PinTarget<'a> {
    pub vibe_home: &'a Path,
    pub roots: &'a [PathBuf],
}

impl PinTarget<'_> {
    fn store_root(&self) -> PathBuf {
        self.vibe_home.join("skills-registry-cache").join("store")
    }

    fn project_manifests(&self) -> Vec<PathBuf> {
        manifest::project_manifest_paths(self.vibe_home, self.roots)
    }

    fn scope_paths(&self, scope: SkillScope) -> Vec<PathBuf> {
        match scope {
            SkillScope::Project => self.project_manifests(),
            SkillScope::Global => vec![manifest::global_manifest_path(self.vibe_home)],
        }
    }

    /// Reference `_manifest_path_for_scope`.
    fn manifest_for_scope(&self, scope: SkillScope) -> Result<PathBuf, RegistrySkillsError> {
        if scope == SkillScope::Global {
            return Ok(manifest::global_manifest_path(self.vibe_home));
        }
        let path = self.project_manifests().into_iter().next().ok_or_else(|| {
            failure("no project skills manifest is available for a project-scoped pin")
        })?;
        if !within_roots(&path, self.roots) {
            return Err(failure(format!(
                "refusing to write a project skills manifest outside the project root: {}",
                path.display()
            )));
        }
        Ok(path)
    }

    /// The pin named `name` in `scope`, with the manifest that holds it.
    fn find(&self, name: &str, scope: SkillScope) -> Option<(ManifestEntry, PathBuf)> {
        self.scope_paths(scope).into_iter().find_map(|path| {
            manifest::load(&path)
                .manifest
                .skills
                .into_iter()
                .find(|entry| entry.name == name)
                .map(|entry| (entry, path))
        })
    }
}

/// One version's registry object, as a details card shows it (reference
/// `SkillDetails`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDetails {
    pub name: String,
    pub skill_id: String,
    pub version: i64,
    pub body: String,
    pub description: String,
    pub created_by: String,
    pub created_at: String,
    pub last_modified_at: String,
    pub sharing_scope: String,
    pub latest_version: i64,
    pub version_created_at: String,
    pub aliases: Vec<String>,
    pub notes: String,
}

fn failure(reason: impl Into<String>) -> RegistrySkillsError {
    RegistrySkillsError {
        reason: reason.into(),
        status: None,
    }
}

fn no_endpoint() -> RegistrySkillsError {
    failure("no authenticated Mistral endpoint available")
}

/// A skill version's details, or [`None`] when there is no endpoint or the
/// registry fails.
pub async fn skill_details(
    endpoint: Option<&RegistryEndpoint>,
    skill_id: &str,
    version: Option<i64>,
) -> Option<SkillDetails> {
    let client = open_client(endpoint?).ok()?;
    let item = client.get_skill(skill_id, version, None).await.ok()?;
    let latest_version = if item.metadata.latest_version != 0 {
        item.metadata.latest_version
    } else {
        item.version
    };
    Some(SkillDetails {
        name: item.resolved_name().unwrap_or_else(|| skill_id.to_owned()),
        skill_id: skill_id.to_owned(),
        version: item.version,
        description: item.resolved_description(),
        body: item.skill.skill_body,
        created_by: item.metadata.created_by,
        created_at: item.metadata.created_at,
        last_modified_at: item.metadata.last_modified_at,
        sharing_scope: item.metadata.sharing_scope,
        latest_version,
        version_created_at: item.version_metadata.created_at,
        aliases: item.version_attributes.aliases,
        notes: item.version_attributes.notes,
    })
}

/// A skill version's body alone.
///
/// # Errors
///
/// No endpoint, or any registry failure.
pub async fn skill_body(
    endpoint: Option<&RegistryEndpoint>,
    skill_id: &str,
    version: Option<i64>,
) -> Result<String, RegistrySkillsError> {
    let client = open_client(endpoint.ok_or_else(no_endpoint)?)?;
    Ok(client
        .get_skill(skill_id, version, None)
        .await?
        .skill
        .skill_body)
}

/// Every published version of a skill, newest first; none when there is no
/// endpoint or the registry fails.
pub async fn skill_versions(
    endpoint: Option<&RegistryEndpoint>,
    skill_id: &str,
) -> Vec<SkillVersionInfo> {
    let Some(endpoint) = endpoint else {
        return Vec::new();
    };
    let Ok(client) = open_client(endpoint) else {
        return Vec::new();
    };
    client.list_versions(skill_id).await.unwrap_or_default()
}

/// Reference `import_skill`: materializes a skill and pins it, to `version`
/// frozen, to a custom `alias`, or to `latest` by default.
///
/// # Errors
///
/// No endpoint, a registry failure, a skill with no usable name or an empty
/// body, or a project scope with no manifest inside the project roots.
pub async fn import_skill(
    endpoint: Option<&RegistryEndpoint>,
    target: &PinTarget<'_>,
    skill_id: &str,
    version: Option<i64>,
    alias: Option<&str>,
    scope: SkillScope,
) -> Result<ManifestEntry, RegistrySkillsError> {
    let client = open_client(endpoint.ok_or_else(no_endpoint)?)?;
    let pin = match (version, alias) {
        (Some(version), _) => ManifestVersion::Frozen(version),
        (None, Some(alias)) => ManifestVersion::Alias(alias.to_owned()),
        (None, None) => ManifestVersion::Alias(REGISTRY_LATEST_ALIAS.to_owned()),
    };
    let item = match &pin {
        ManifestVersion::Frozen(version) => {
            client.get_skill(skill_id, Some(*version), None).await?
        }
        ManifestVersion::Alias(alias) => client.get_skill(skill_id, None, Some(alias)).await?,
    };
    let name = item
        .resolved_name()
        .ok_or_else(|| failure(format!("skill {skill_id} has no usable name")))?;
    let stored = store::materialize(&target.store_root(), &item, &name)
        .map_err(|error| failure(error.to_string()))?;
    if stored.is_none() {
        return Err(failure(format!("skill {skill_id} has an empty body")));
    }
    if let ManifestVersion::Alias(alias) = &pin
        && alias != REGISTRY_LATEST_ALIAS
    {
        record_resolved(target.vibe_home, skill_id, alias, item.version);
    }
    let path = target.manifest_for_scope(scope)?;
    let mut manifest = manifest::load(&path).manifest;
    let entry = ManifestEntry {
        name,
        skill_id: skill_id.to_owned(),
        version: pin,
        description: item.resolved_description(),
    };
    manifest.upsert(entry.clone());
    manifest::save(&path, &manifest).map_err(|error| failure(error.to_string()))?;
    Ok(entry)
}

/// Reference `set_skill_version`, `set_skill_latest` and `set_skill_alias`:
/// the pin named `name` in `scope` re-imported at a new pin, or nothing when
/// the scope pins no such skill.
///
/// # Errors
///
/// What [`import_skill`] answers.
pub async fn repin_skill(
    endpoint: Option<&RegistryEndpoint>,
    target: &PinTarget<'_>,
    name: &str,
    version: Option<i64>,
    alias: Option<&str>,
    scope: SkillScope,
) -> Result<Option<ManifestEntry>, RegistrySkillsError> {
    let Some((entry, _)) = target.find(name, scope) else {
        return Ok(None);
    };
    import_skill(endpoint, target, &entry.skill_id, version, alias, scope)
        .await
        .map(Some)
}

/// Reference `remove_skill`: the pin removed from `scope` only, answering
/// whether one was there.
///
/// # Errors
///
/// A manifest that cannot be written.
pub fn remove_skill(
    target: &PinTarget<'_>,
    name: &str,
    scope: SkillScope,
) -> std::io::Result<bool> {
    let paths = match scope {
        SkillScope::Project => target
            .project_manifests()
            .into_iter()
            .filter(|path| within_roots(path, target.roots))
            .collect(),
        SkillScope::Global => vec![manifest::global_manifest_path(target.vibe_home)],
    };
    let mut removed = false;
    for path in paths {
        let mut manifest: SkillManifest = manifest::load(&path).manifest;
        if manifest.remove(name) {
            manifest::save(&path, &manifest)?;
            removed = true;
        }
    }
    Ok(removed)
}

/// Reference `convert_skill_to_local`: a pin's materialized content copied
/// into the local skills directory of its scope as a standalone skill, and
/// the pin removed. [`None`] when nothing is materialized, the name is not a
/// plain path component, or a local skill of that name already exists.
///
/// # Errors
///
/// A copy or manifest write that fails.
pub fn convert_skill_to_local(
    target: &PinTarget<'_>,
    name: &str,
    scope: SkillScope,
) -> std::io::Result<Option<PathBuf>> {
    let Some((entry, manifest_path)) = target.find(name, scope) else {
        return Ok(None);
    };
    let root = target.store_root();
    let Some(version) = resolved_version(target.vibe_home, &root, &entry) else {
        return Ok(None);
    };
    let materialized = store::skill_dir(&root, &entry.skill_id, version)
        .is_ok_and(|directory| directory.join("SKILL.md").is_file());
    if !materialized || !store::is_plain_component(name) {
        return Ok(None);
    }
    let destination = match scope {
        SkillScope::Global => target.vibe_home.join("skills").join(name),
        SkillScope::Project => {
            let base = manifest_path
                .parent()
                .map_or_else(|| manifest_path.clone(), Path::to_path_buf);
            let destination = base.join("skills").join(name);
            if !within_roots(&destination, target.roots) {
                return Ok(None);
            }
            destination
        }
    };
    if destination.exists() {
        return Ok(None);
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Err(error) = store::export_local(&root, &entry.skill_id, version, &destination) {
        let _ = std::fs::remove_dir_all(&destination);
        return Err(match error {
            store::StoreError::Io(error) => error,
            other => std::io::Error::other(other.to_string()),
        });
    }
    remove_skill(target, name, scope)?;
    Ok(Some(destination))
}

/// Reference `_resolved_version`: the materialized version a pin maps to.
fn resolved_version(vibe_home: &Path, root: &Path, entry: &ManifestEntry) -> Option<i64> {
    let latest = || {
        store::latest_materialized(root, &entry.skill_id)
            .ok()
            .flatten()
    };
    match &entry.version {
        ManifestVersion::Frozen(version) => Some(*version),
        ManifestVersion::Alias(alias) if alias == REGISTRY_LATEST_ALIAS => latest(),
        ManifestVersion::Alias(alias) => resolved_alias(vibe_home, &entry.skill_id, alias)
            .filter(|version| {
                store::skill_dir(root, &entry.skill_id, *version)
                    .is_ok_and(|directory| directory.join("SKILL.md").is_file())
            })
            .or_else(latest),
    }
}

/// Reference `_within_project_roots`: `path` resolved lies under a root.
fn within_roots(path: &Path, roots: &[PathBuf]) -> bool {
    let resolved = resolve_lenient(path);
    roots
        .iter()
        .any(|root| resolved.starts_with(resolve_lenient(root)))
}

/// A path resolved through its longest existing prefix.
fn resolve_lenient(path: &Path) -> PathBuf {
    if let Ok(resolved) = std::fs::canonicalize(path) {
        return resolved;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => resolve_lenient(parent).join(name),
        _ => path.to_path_buf(),
    }
}

fn cache_path(vibe_home: &Path) -> PathBuf {
    vibe_home.join("cache.toml")
}

fn resolved_alias(vibe_home: &Path, skill_id: &str, alias: &str) -> Option<i64> {
    let text = std::fs::read_to_string(cache_path(vibe_home)).ok()?;
    let document = text.parse::<toml::Table>().ok()?;
    document
        .get(RESOLVED_SECTION)?
        .get(format!("{skill_id}@{alias}"))?
        .as_integer()
}

/// Reference `_resolved.record`: merged into the section, a write that fails
/// dropped as the reference logs and drops it.
fn record_resolved(vibe_home: &Path, skill_id: &str, alias: &str, version: i64) {
    let path = cache_path(vibe_home);
    let mut document = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .unwrap_or_default();
    let section = document
        .entry(RESOLVED_SECTION)
        .or_insert_with(|| toml::Value::Table(toml::Table::new()));
    if !section.is_table() {
        *section = toml::Value::Table(toml::Table::new());
    }
    if let Some(section) = section.as_table_mut() {
        section.insert(format!("{skill_id}@{alias}"), toml::Value::Integer(version));
    }
    if let Ok(encoded) = toml::to_string(&document) {
        let _ = std::fs::create_dir_all(vibe_home);
        let _ = std::fs::write(&path, encoded);
    }
}
