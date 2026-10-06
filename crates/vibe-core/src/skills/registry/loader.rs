//! The registry's skills as a session reads them.
//!
//! Reference `SkillManager._registry_sources`, `_discover_registry_skills`,
//! `registry_pins` and `_load_registry_entry` (`vibe/core/skills/manager.py`):
//! the global manifest is read first and every project manifest after it, each
//! pin is resolved to a materialized version (a frozen number as written,
//! `latest` to the newest version on disk, a custom alias to the version its
//! last refresh recorded, else the newest on disk), and the generated
//! `SKILL.md` is loaded like any other skill, tagged with its registry
//! provenance. Nothing here reaches the network: a pin whose body was never
//! materialized is skipped until a sync writes it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::manifest::{self, ManifestEntry, ManifestVersion, REGISTRY_LATEST_ALIAS};
use super::{pins, store};
use crate::extensions::{DiscoveryIssue, SkillDefinition, load_skill};
use crate::skills::{RegistryRef, RegistrySources, SkillScope, SkillSource};

/// The skills a set of pins loaded, and the issues their files raised.
#[derive(Debug, Clone, Default)]
pub struct LoadedRegistry {
    pub skills: BTreeMap<String, SkillDefinition>,
    pub issues: Vec<DiscoveryIssue>,
}

/// Reference `_registry_sources`: the global manifest, then the project
/// manifests of the open roots, each with the scope its pins publish.
#[must_use]
pub fn manifest_sources(sources: &RegistrySources) -> Vec<(PathBuf, SkillScope)> {
    let mut paths = vec![(
        manifest::global_manifest_path(&sources.vibe_home),
        SkillScope::Global,
    )];
    paths.extend(
        manifest::project_manifest_paths(&sources.vibe_home, &sources.project_roots)
            .into_iter()
            .map(|path| (path, SkillScope::Project)),
    );
    paths
}

/// Reference `_discover_registry_skills`: the active set a session loads, one
/// skill per pinned name, a project pin winning over a global one.
#[must_use]
pub fn active_skills(sources: &RegistrySources) -> LoadedRegistry {
    let mut loaded = LoadedRegistry::default();
    for (path, scope) in manifest_sources(sources) {
        for entry in manifest::load(&path).manifest.skills {
            if let Some(skill) = load_entry(&sources.vibe_home, &entry, scope, &mut loaded.issues) {
                loaded.skills.insert(entry.name.clone(), skill);
            }
        }
    }
    loaded
}

/// Reference `registry_pins`: every pin as its own skill, one per name and
/// scope, so a skill pinned globally and in the project is two rows a browser
/// manages separately. The order is the first appearance of each pair.
#[must_use]
pub fn pinned_skills(sources: &RegistrySources) -> (Vec<SkillDefinition>, Vec<DiscoveryIssue>) {
    let mut issues = Vec::new();
    let mut rows: Vec<((String, SkillScope), SkillDefinition)> = Vec::new();
    for (path, scope) in manifest_sources(sources) {
        for entry in manifest::load(&path).manifest.skills {
            let Some(skill) = load_entry(&sources.vibe_home, &entry, scope, &mut issues) else {
                continue;
            };
            let key = (entry.name.clone(), scope);
            match rows.iter_mut().find(|(seen, _)| *seen == key) {
                Some((_, row)) => *row = skill,
                None => rows.push((key, skill)),
            }
        }
    }
    (rows.into_iter().map(|(_, skill)| skill).collect(), issues)
}

/// Reference `_load_registry_entry`: the skill one pin loads, or [`None`]
/// when its id is not a safe path segment or no version of it is on disk.
fn load_entry(
    vibe_home: &Path,
    entry: &ManifestEntry,
    scope: SkillScope,
    issues: &mut Vec<DiscoveryIssue>,
) -> Option<SkillDefinition> {
    if !store::is_plain_component(&entry.skill_id) {
        return None;
    }
    let root = store::store_root(vibe_home);
    let version = pinned_version(vibe_home, &root, entry)?;
    let skill_file = store::skill_dir(&root, &entry.skill_id, version)
        .ok()?
        .join("SKILL.md");
    if !skill_file.is_file() {
        return None;
    }
    let mut skill = load_skill(&skill_file, SkillSource::Registry, scope, issues)?;
    skill.registry = Some(RegistryRef {
        skill_id: entry.skill_id.clone(),
        version,
        alias: entry.alias().map(ToOwned::to_owned),
    });
    Some(skill)
}

/// The concrete version a pin loads: a frozen number as written, `latest` the
/// newest materialized version, and a custom alias the version its last
/// refresh recorded when that one is on disk, else the newest on disk.
pub(crate) fn pinned_version(vibe_home: &Path, root: &Path, entry: &ManifestEntry) -> Option<i64> {
    let latest = || {
        store::latest_materialized(root, &entry.skill_id)
            .ok()
            .flatten()
    };
    match &entry.version {
        ManifestVersion::Frozen(version) => Some(*version),
        ManifestVersion::Alias(alias) if alias == REGISTRY_LATEST_ALIAS => latest(),
        ManifestVersion::Alias(alias) => pins::resolved_alias(vibe_home, &entry.skill_id, alias)
            .filter(|version| store::is_materialized(root, &entry.skill_id, *version))
            .or_else(latest),
    }
}
